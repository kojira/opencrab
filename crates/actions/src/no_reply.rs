//! `NO_REPLY` 終端解釈の配送層フック（DESIGN-RESUME-SETTLE §3.1 / §3.1.1・第一柱）。
//!
//! 純粋な終端判定（[`terminate_at_no_reply`] / [`NoReplyTermination`]）は **`opencrab_core`
//! が単一実装**として持つ（継続を判定する engine が core に居り、NO_REPLY / CONTINUE の両
//! センチネルと判定を core が一元管理する・#890 §11.5・#916 レビュー）。ここは互換のため
//! re-export し、配送層固有の破棄ログ（[`log_trailing_discard`]・[`DeliveryContext`]）だけを持つ。
//!
//! R4 統括裁定: **出現＝終端**（例外規則なし・文中引用も終端扱い）。応答に最初の `NO_REPLY`
//! が現れた地点で発言を打ち切り、前段が空なら沈黙・非空なら前段のみを発言・以降は破棄する。
//! 破棄内容は後続に非空テキストがある場合だけ固定タグ [`NO_REPLY_TRAILING_DISCARDED_TAG`] の
//! WARN をサーバローカルログへ残す（wire・配送・gateway 通知には載せない）。

// 純粋判定と結果型・センチネルは core の単一実装を re-export する（別実装を作らない）。
pub use opencrab_core::continue_marker::{
    terminate_at_no_reply, NoReplyTermination, NO_REPLY_SENTINEL,
};

/// 破棄ログの固定タグ（`grep -c` で頻度集計できるよう 1 語不変・§3.1.1(b)）。
pub const NO_REPLY_TRAILING_DISCARDED_TAG: &str = "no_reply_trailing_discarded";

/// 破棄ログの tracing target（qc harness 等が拾う識別子）。
pub const NO_REPLY_LOG_TARGET: &str = "opencrab::no_reply";

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

/// §3.1.1: `NO_REPLY` の後に非空テキストが続いていたら破棄ログを WARN で残す。
///
/// 破棄テキストは **サーバローカルログのみ**（wire・配送・gateway 通知には載せない）。
/// 単独 `NO_REPLY`・末尾 `NO_REPLY`（後続が空白のみ）では WARN を出さない。
/// core の [`NoReplyTermination`] を受け取る配送層固有の副作用（tracing）なのでここに置く。
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

    /// 破棄ログの発火条件（後続非空だけ Some）を core の trailing_discard 経由で確認する。
    #[test]
    fn log_trailing_discard_smoke() {
        // 末尾 NO_REPLY（後続空）は破棄ログ対象外。
        let quiet = terminate_at_no_reply("本文だけ話す NO_REPLY");
        assert_eq!(quiet.trailing_discard(), None);
        log_trailing_discard(&quiet, ctx()); // 出さない（パニックしないこと）。
                                             // 後続非空は破棄対象。
        let noisy = terminate_at_no_reply("本文 NO_REPLY ゴミ");
        assert_eq!(noisy.trailing_discard(), Some("NO_REPLY ゴミ"));
        log_trailing_discard(&noisy, ctx()); // WARN を出す（パニックしないこと）。
    }
}
