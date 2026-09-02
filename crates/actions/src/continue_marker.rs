//! 配送層の `CONTINUE` マーカー処理（DESIGN-TURN-CONTINUATION §11・#890）。
//!
//! マーカー剥がしの純粋ロジックは `opencrab_core::continue_marker` にある（継続を判定する
//! engine が core に居り、`actions → core` の一方向依存のため core 側が単一実装。§11.5 の
//! 「no_reply.rs の隣」からの配置差分は core 側モジュールの doc を参照）。ここは配送 3 箇所
//! （`session_inbound::delivery_effect` / discord `on_response_text` / nostr 完了 sink）が
//! 共有する「NO_REPLY 終端 → CONTINUE 末尾剥がし」の単一実装を提供する。

pub use opencrab_core::continue_marker::{
    strip_continue_marker, ContinueMarker, CONTINUE_LOG_TARGET, CONTINUE_MIDTEXT_TAG,
    CONTINUE_SENTINEL,
};

use crate::no_reply::{terminate_at_no_reply, DeliveryContext};

/// 配送前の可視テキストを 1 経路で確定する（3 配送点の単一実装）。
///
/// 1. `NO_REPLY` 終端解釈（第一柱・R4）。前段が空なら「沈黙」で `None`。
/// 2. 残った発言本文の末尾 `CONTINUE` を剥がす（§11・継続判定は engine が済ませているので
///    ここは表示保護。末尾以外の出現は剥がさず WARN のみ）。
///
/// `None` は沈黙（`NoReply`）。`Some` はユーザーへ出す本文。
pub fn visible_speech_after_markers(raw: &str, ctx: DeliveryContext<'_>) -> Option<String> {
    let term = terminate_at_no_reply(raw);
    term.log_trailing_discard(ctx);
    let speech = term.speech()?;
    let marker = strip_continue_marker(speech);
    marker.log_midtext(ctx.session_id, ctx.agent_id, ctx.origin);
    let visible = if marker.at_tail() {
        marker.kept()
    } else {
        speech
    };
    Some(visible.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::no_reply::NO_REPLY_SENTINEL;

    fn ctx() -> DeliveryContext<'static> {
        DeliveryContext {
            session_id: "s1",
            agent_id: "a1",
            origin: "test",
        }
    }

    /// core 側のミラー定数が actions の正本と一致すること（複製ドリフト防止）。
    #[test]
    fn no_reply_sentinel_mirror_matches() {
        assert_eq!(
            opencrab_core::continue_marker::NO_REPLY_SENTINEL,
            NO_REPLY_SENTINEL,
            "core の NO_REPLY ミラーは actions の正本と一致しなければならない"
        );
    }

    /// 配送点 session_inbound 相当: 末尾 CONTINUE を剥がして本文だけ配送する。
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
        // 前段が空 + NO_REPLY → 沈黙。末尾 CONTINUE は関与しない。
        let out = visible_speech_after_markers("NO_REPLY\nCONTINUE", ctx());
        assert_eq!(out, None, "NO_REPLY 優先で沈黙");
    }

    /// NO_REPLY 前段が非空なら前段のみ・CONTINUE は NO_REPLY 以降なので破棄済み。
    #[test]
    fn no_reply_keeps_leading_body_and_drops_continue() {
        let out = visible_speech_after_markers("本文だけ話す NO_REPLY CONTINUE", ctx());
        assert_eq!(out.as_deref(), Some("本文だけ話す"));
    }

    /// 途中出現の CONTINUE は剥がさず本文のまま残す（WARN のみ）。
    #[test]
    fn midtext_continue_is_kept() {
        let text = "まず CONTINUE を確認してから続けます";
        let out = visible_speech_after_markers(text, ctx());
        assert_eq!(out.as_deref(), Some(text));
    }
}
