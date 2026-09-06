
/// #504: 文脈から切り離された「飛び地」のユーザー発言は会話へ載せない。
///
/// #284 は「直近ユーザー発言が 1 件も載らない」事故を、末尾の連続区間から外れた
/// ユーザー発言を省略マーカーで挟んだ**飛び地**として個別に載せることで防いでいた。
/// だが飛び地は文脈も応答有無も失われた裸の発言で、オーナー判断は「無いより悪い」。
///
/// そこで A′: **一番新しいユーザー発言 1 件だけは飛び地でも必ず載せ**（＝「今の指示」で、
/// #284 が本当に守りたかったもの）、それより古い飛び地は落とす。落とした分は件数と
/// 期間を書いた省略マーカーに集約する（`format_omission_marker`）。
///
/// ここは [`super::fit_logs_to_budget`] を直接叩き、行の index と `created_at` を固定して
/// 判定する（DB や予算経路の間接を挟むと、どの発言が飛び地になるかが読みにくい）。
#[cfg(test)]
mod orphan_user_speech_tests {
    use super::fit_logs_to_budget;
    use opencrab_db::queries::SessionLogRow;

    const AGENT: &str = "a1";
    const USER: &str = "owner";
    const SESSION: &str = "s1";

    fn user_speech(content: &str, created_at: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(), // 受信側エージェント（#377）
            session_id: SESSION.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(USER.to_string()), // 送信者 ≠ AGENT → is_user_speech 真
            turn_number: None,
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        }
    }

    fn tool_result(content: &str, created_at: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            log_type: "tool_result".to_string(),
            content: content.to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        }
    }

    /// 連続区間の外にユーザー発言 3 件（8/1・8/6・8/6）を置き、末尾を巨大なツール結果で
    /// 埋め尽くして 3 件すべてを飛び地にする。index の大きい "NEWEST" が一番新しい発言。
    fn orphaned_speeches_then_tool_flood() -> Vec<SessionLogRow> {
        let mut logs = vec![
            user_speech("OLD-A-最古の飛び地", "2026-08-01T00:00:00Z"),
            user_speech("OLD-B-古い飛び地", "2026-08-06T00:00:00Z"),
            user_speech("NEWEST-一番新しい指示", "2026-08-06T12:00:00Z"),
        ];
        // 末尾の連続区間を埋め、予算を使い切らせる（＝上の 3 件を連続区間の外に押し出す）。
        for i in 0..20 {
            logs.push(tool_result(
                &format!("tool output {i}: {}", "data ".repeat(200)),
                "2026-08-06T12:00:00Z",
            ));
        }
        logs
    }

    /// 一番新しいユーザー発言だけは飛び地でも残り、それより古い飛び地は消える。
    #[test]
    fn only_the_newest_orphan_user_speech_is_kept() {
        let logs = orphaned_speeches_then_tool_flood();
        let out = fit_logs_to_budget(&logs, AGENT, 300);
        assert!(
            out.contains("NEWEST-一番新しい指示"),
            "一番新しいユーザー発言が飛び地でも残っていない: {out}"
        );
        assert!(
            !out.contains("OLD-A-最古の飛び地"),
            "古い飛び地が消えていない（OLD-A）: {out}"
        );
        assert!(
            !out.contains("OLD-B-古い飛び地"),
            "古い飛び地が消えていない（OLD-B）: {out}"
        );
    }

    /// 落とした古い区間は、件数と期間を添えた省略マーカーに集約される。
    #[test]
    fn omission_marker_carries_count_and_period() {
        let logs = orphaned_speeches_then_tool_flood();
        let out = fit_logs_to_budget(&logs, AGENT, 300);
        // 一番新しい発言(index 2)より前の飛び地 = index 0..2（OLD-A 8/1・OLD-B 8/6）。
        // 件数 2・期間 5 日がマーカーに出る。
        assert!(
            out.contains("2 older messages over 5 days"),
            "省略マーカーに件数・期間が入っていない: {out}"
        );
    }
}
