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

/// 応答の最終行全体が `NO_REPLY` の場合だけ終端解釈する。
/// 文中・引用内・途中行の `NO_REPLY` は通常の発話としてそのまま保持する。
pub fn terminate_at_no_reply(response: &str) -> NoReplyTermination {
    let without_trailing_whitespace = response.trim_end();
    let line_start = without_trailing_whitespace
        .rfind('\n')
        .map_or(0, |idx| idx + 1);
    let final_line = &without_trailing_whitespace[line_start..];

    if final_line.trim() != NO_REPLY_SENTINEL {
        return NoReplyTermination {
            kept: response.to_string(),
            discarded: None,
        };
    }

    let mut kept_end = line_start.saturating_sub(1);
    if response[..kept_end].ends_with('\r') {
        kept_end -= 1;
    }
    let marker_start = line_start
        + final_line
            .find(NO_REPLY_SENTINEL)
            .expect("trimmed final line equals sentinel");
    NoReplyTermination {
        kept: response[..kept_end].to_string(),
        discarded: Some(response[marker_start..].to_string()),
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

    /// 旧「マーカー以降を破棄」契約との互換API。
    /// 最終独立行だけを終端とする現在の parser では常に `None` を返す。
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
    fn observed_labomi_response_keeps_quoted_no_reply_verbatim() {
        let raw = "そうだったんだね。  \n「返答する」と「NO_REPLYで終わる」を同時に強く指示するような矛盾が、再生成や重複の引き金になってたんだ。原因が見えて、今度こそすっきりしたね〜⚡";
        let termination = terminate_at_no_reply(raw);
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some(raw));
        assert_eq!(termination.kept(), raw);
    }

    #[test]
    fn observed_nostaro_response_removes_only_final_no_reply_line() {
        let visible = "うん。**「返事する」と「NO_REPLYで止める」の条件が衝突して、終了判定が揺れてた**んだね。  \nモデルの癖というより、矛盾した指示に忠実であろうとして再応答してたわけか。そりゃモデル変更だけじゃ直りきらないじゃん⚡";
        let raw = format!("{visible}\n\nNO_REPLY");
        let termination = terminate_at_no_reply(&raw);
        assert!(termination.terminated());
        assert_eq!(termination.speech(), Some(visible));
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn body_then_standalone_final_no_reply_keeps_body() {
        let termination = terminate_at_no_reply("本文だけ話す\nNO_REPLY");
        assert!(termination.terminated());
        assert_eq!(termination.speech(), Some("本文だけ話す"));
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn final_no_reply_line_allows_surrounding_whitespace() {
        let termination = terminate_at_no_reply("本文だけ話す\r\n  NO_REPLY  \r\n");
        assert!(termination.terminated());
        assert_eq!(termination.speech(), Some("本文だけ話す"));
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn inline_no_reply_is_ordinary_speech() {
        let text = "本文中の NO_REPLY は説明の一部";
        let termination = terminate_at_no_reply(text);
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some(text));
        assert_eq!(termination.kept(), text);
    }

    #[test]
    fn quoted_no_reply_is_ordinary_speech() {
        let text = "「返答する」と「NO_REPLYで終わる」の条件を整理する";
        let termination = terminate_at_no_reply(text);
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some(text));
        assert_eq!(termination.kept(), text);
    }

    #[test]
    fn standalone_no_reply_before_a_later_line_is_ordinary_speech() {
        let text = "本文\nNO_REPLY\n後続の説明";
        let termination = terminate_at_no_reply(text);
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some(text));
        assert_eq!(termination.kept(), text);
    }

    #[test]
    fn final_line_with_extra_text_is_ordinary_speech() {
        let text = "本文\nNO_REPLY まだ続く";
        let termination = terminate_at_no_reply(text);
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some(text));
        assert_eq!(termination.kept(), text);
    }
}
