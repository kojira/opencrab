//! `NO_REPLY` 終端解釈の配送層フック（DESIGN-RESUME-SETTLE §3.1 / §3.1.1・第一柱）。
//!
//! 純粋な終端判定（[`terminate_at_no_reply`] / [`NoReplyTermination`]）は **`opencrab_core`
//! が単一実装**として持つ（継続と明示終端を判定する engine が core に居り、NO_REPLY の
//! センチネルと判定を一元管理する）。ここは互換のため
//! re-export し、配送層固有の破棄ログ（[`log_trailing_discard`]・[`DeliveryContext`]）だけを持つ。
//!
//! `NO_REPLY` は、応答の最終行全体がその文字列だけの場合に限り終端として扱う。
//! 文中・引用内・途中行の出現は通常の発話として保持する。

// 純粋判定と結果型・センチネルは core の単一実装を re-export する（別実装を作らない）。
pub use opencrab_core::continue_marker::{
    terminate_at_no_reply, NoReplyTermination, NO_REPLY_LOG_TARGET, NO_REPLY_SENTINEL,
};

/// 旧「マーカー以降を破棄」契約との互換用ログタグ。
pub const NO_REPLY_TRAILING_DISCARDED_TAG: &str = "no_reply_trailing_discarded";

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

/// 旧「マーカー以降を破棄」契約との互換フック。
/// 最終独立行だけを終端とする現在の parser では破棄対象が生じないため、通常は何もしない。
pub fn log_trailing_discard(term: &NoReplyTermination, ctx: DeliveryContext<'_>) {
    let Some(discarded) = term.trailing_discard() else {
        return;
    };
    tracing::warn!(
        target: NO_REPLY_LOG_TARGET,
        discarded = %discarded,
        discarded_len = discarded.chars().count(),
        kept_len = term.kept().trim().chars().count(),
        session_id = %ctx.session_id,
        agent_id = %ctx.agent_id,
        origin = %ctx.origin,
        "no_reply_trailing_discarded"
    );
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
    fn final_line_marker_has_no_trailing_discard() {
        let termination = terminate_at_no_reply("本文だけ話す\nNO_REPLY");
        assert!(termination.terminated());
        assert_eq!(termination.trailing_discard(), None);
        log_trailing_discard(&termination, ctx());
    }
}
