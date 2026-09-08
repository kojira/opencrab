//! 配送層の `CONTINUE` マーカー処理（DESIGN-TURN-CONTINUATION §11 / §11.7・#890）。
//!
//! マーカー剥がしの純粋ロジックは `opencrab_core::continue_marker` にある（継続を判定する
//! engine が core に居り、`actions → core` の一方向依存のため core 側が単一実装。§11.5 の
//! 「no_reply.rs の隣」からの配置差分は core 側モジュールの doc を参照）。ここは配送 3 箇所
//! （`session_inbound::delivery_effect` / discord `on_response_text` / nostr 完了 sink）が
//! 共有する「NO_REPLY 終端 → CONTINUE 末尾剥がし」の単一フックを提供する。

pub use opencrab_core::continue_marker::{
    strip_trailing_continue, CONTINUE_LOG_TARGET, CONTINUE_SENTINEL,
};

use crate::no_reply::{terminate_at_no_reply, DeliveryContext};

/// 配送前の可視テキストを 1 経路で確定する（3 配送点の単一フック）。
///
/// 1. `NO_REPLY` 終端解釈（第一柱・R4）。前段が空なら「沈黙」で `None`。
/// 2. 残った発言本文の最終行が `CONTINUE` 単独なら剥がす（§11.7・継続判定は engine が済ませ、
///    ここは表示保護）。最終行単独でない `CONTINUE`（同一行併記・途中出現）は剥がさず、解析用に
///    WARN を残す。
///
/// `None` は沈黙（`NoReply`）。`Some` はユーザーへ出す本文。
pub fn visible_speech_after_markers(raw: &str, ctx: DeliveryContext<'_>) -> Option<String> {
    let term = terminate_at_no_reply(raw);
    crate::no_reply::log_trailing_discard(&term, ctx);
    let speech = term.speech()?;
    if let Some(body) = strip_trailing_continue(speech) {
        return Some(body.to_string());
    }
    // 最終行単独でないのに CONTINUE が本文に残っていれば WARN（同一行併記・途中出現）。
    if speech.contains(CONTINUE_SENTINEL) {
        tracing::warn!(
            target: CONTINUE_LOG_TARGET,
            session_id = %ctx.session_id,
            agent_id = %ctx.agent_id,
            origin = %ctx.origin,
            "continue_marker_midtext"
        );
    }
    Some(speech.to_string())
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

    /// 配送点 session_inbound 相当: 最終行 CONTINUE 単独を剥がして本文だけ配送する。
    #[test]
    fn session_inbound_strips_tail_continue() {
        let out = visible_speech_after_markers("確認して返すね⚡\nCONTINUE", ctx());
        assert_eq!(out.as_deref(), Some("確認して返すね⚡"));
    }

    /// 配送点 discord 相当: マーカー無しは本文そのまま。
    #[test]
    fn discord_passes_through_without_marker() {
        let out = visible_speech_after_markers("普通の返信です", ctx());
        assert_eq!(out.as_deref(), Some("普通の返信です"));
    }

    /// 配送点 nostr 相当: NO_REPLY 優先（CONTINUE が同居しても沈黙）。
    #[test]
    fn nostr_no_reply_wins_over_continue() {
        let out = visible_speech_after_markers("NO_REPLY\nCONTINUE", ctx());
        assert_eq!(out, None, "NO_REPLY 優先で沈黙");
    }

    /// §11.7: 同一行に他の文字がある CONTINUE は剥がさず本文のまま残す。
    #[test]
    fn same_line_continue_is_kept() {
        let text = "確認して返信します CONTINUE";
        let out = visible_speech_after_markers(text, ctx());
        assert_eq!(out.as_deref(), Some(text));
    }

    /// 途中出現の CONTINUE も剥がさず本文のまま残す。
    #[test]
    fn midtext_continue_is_kept() {
        let text = "まず CONTINUE を確認してから続けます";
        let out = visible_speech_after_markers(text, ctx());
        assert_eq!(out.as_deref(), Some(text));
    }
}
