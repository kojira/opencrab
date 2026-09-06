
/// #500: 組み上がった会話が予算で頭打ちになる（＝コンパクションが機能している）ことの
/// **回復ガード**。
///
/// **これは heartbeat 障害の再発防止ではない。** あの日は会話が予算（525,000）に収まって
/// いたのに、その予算自体がバックエンドの実上限（371,678）を超えていた。破れたのは
/// 「予算 ⇔ バックエンド実上限」の間で、そこには今も天井が無く **#535 の管轄**（本番は
/// `context_window` を手で下げているだけ）。ここが守るのは「budget が正しく設定されている
/// 前提で、履歴がいくら伸びてもコンパクションが出力を予算付近まで頭打ちにすること」だけ。
///
/// topic 圧縮経路（[`past_summary_budget_tests::recent_conversation_keeps_its_share_when_topics_are_huge`]）
/// とハートビート経路（[`past_summary_budget_tests::heartbeat_total_stays_within_budget_and_keeps_channel_and_format_instruction`]）
/// の「出力 ≤ 予算」は #406 で既に固定済み。ここは**未カバーだった topic 無しの切り詰め
/// フォールバック**（[`super::build_recent_window`]）を埋める。
///
/// なお `fit_logs_to_budget` は末尾から予算いっぱいまで詰めるので出力はほぼ予算ちょうどに
/// なり、**予算に計上されない省略マーカー / セクション区切りのぶん、通常サイズの行でも
/// 出力は予算を数十トークンだけ超えうる**（実測で +12 程度。#536 の巨大行による超過とは
/// 別の、境界の小さなオーバーヘッド）。回復ガードが見たいのは「履歴全体＝予算の数倍まで
/// 膨らまない」ことなので、予算＋小さな既知の余白で判定する。
#[cfg(test)]
mod budget_fit_recovery_guard_tests {
    use super::{build_conversation_string, estimate_tokens};

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    fn insert_speech(conn: &rusqlite::Connection, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(AGENT.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// topic 要約が 1 件も無い（＝`build_recent_window` フォールバック）状態で
    /// 履歴が予算を大きく超えても、組み上がった会話は予算内に収まる。
    ///
    /// 既存の fits テストは全て topic ありの summary 経路。topic 生成が追いつく前の
    /// セッションや要約が引けない経路はこの切り詰めフォールバックへ落ちるため、そこも
    /// 頭打ちになることを固定する。行は通常サイズ（下限が巨大行で予算を割る #536 の
    /// floor 経路とは別条件）。
    ///
    /// #536: 省略マーカーを予算へ計上したので、以前は必要だった余白（`MARKER_SLACK`）を
    /// 外し、**厳密に `<= BUDGET`** で固定する。
    #[test]
    fn truncated_fallback_without_topics_stays_within_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        // 予算を大きく超える履歴を積む（topic は入れない → 切り詰め経路）。
        for i in 0..400 {
            insert_speech(
                &conn,
                &format!("log line {i} about the release plan and the follow-up work"),
            );
        }

        const BUDGET: usize = 4_000;

        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        assert!(
            !out.contains("[Past context summary"),
            "廃止した topic 要約が出ている: {out}"
        );
        assert!(
            out.contains("[old_history_summary]"),
            "切り詰めの注記が無い＝コンパクションが起きていない: {out}"
        );
        let toks = estimate_tokens(&out);
        assert!(
            toks <= BUDGET,
            "切り詰め経路で出力が予算 {BUDGET} を超えた: {toks} トークン。\
             省略マーカーの計上（#536）が効いていない可能性がある"
        );
    }

    /// #536 の回帰ガード: 省略マーカーを予算へ計上する前は、通常サイズの行でも複数の
    /// 予算値で出力が予算を数十トークン超えていた（実測 budget=6,000 で +12、10,000 で
    /// +11）。マーカー計上後は**どの予算でも厳密に `<= budget`**。マーカーが実際に出る
    /// （コンパクションが起きる）予算帯を複数点で固定する。
    #[test]
    fn omission_markers_are_counted_so_output_never_exceeds_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        // 全文が下の最大予算（20,000）も超えるだけの通常サイズ行を積む（topic 無し →
        // 切り詰め経路）。どの予算でもコンパクション（＝マーカー）が起きるようにする。
        for i in 0..2_000 {
            insert_speech(
                &conn,
                &format!("log line {i} about the release plan and the follow-up work"),
            );
        }
        // マーカーが出る（＝コンパクションが起きる）予算帯を複数点で。#536 前は
        // 6,000 / 10,000 で超過していた。
        for budget in [2_000usize, 4_000, 6_000, 8_000, 10_000, 20_000] {
            let out = build_conversation_string(&conn, SESSION, AGENT, budget).unwrap();
            assert!(
                out.contains("[old_history_summary]"),
                "budget={budget} でコンパクションが起きていない: {out}"
            );
            let toks = estimate_tokens(&out);
            assert!(
                toks <= budget,
                "budget={budget} で出力が予算を超えた: {toks} トークン（+{}）。\
                 マーカー計上（#536）の回帰",
                toks - budget
            );
        }
    }
}
