//! 配送前の明示終端処理。
//!
//! `NO_REPLY` より前の本文だけを配送し、終端記号自体は表示しない。終了事実の永続化は
//! server の turn 終了点が `turn_terminated` system event として一度だけ行う。

use crate::no_reply::{terminate_at_no_reply, DeliveryContext};

/// 配送前の可視テキストを確定する。
///
/// `NO_REPLY` より前に本文があればその本文、単独 `NO_REPLY` なら `None`。
pub fn visible_speech_after_markers(raw: &str, ctx: DeliveryContext<'_>) -> Option<String> {
    let term = terminate_at_no_reply(raw);
    crate::no_reply::log_trailing_discard(&term, ctx);
    term.speech().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> DeliveryContext<'static> {
        DeliveryContext {
            session_id: "s1",
            agent_id: "a1",
            origin: "test",
        }
    }

    #[test]
    fn plain_speech_passes_through() {
        let out = visible_speech_after_markers("普通の返信です", ctx());
        assert_eq!(out.as_deref(), Some("普通の返信です"));
    }

    #[test]
    fn standalone_no_reply_is_not_delivered() {
        assert_eq!(visible_speech_after_markers("NO_REPLY", ctx()), None);
    }

    #[test]
    fn body_before_no_reply_is_delivered() {
        let out = visible_speech_after_markers("最終回答\nNO_REPLY", ctx());
        assert_eq!(out.as_deref(), Some("最終回答"));
    }
}
