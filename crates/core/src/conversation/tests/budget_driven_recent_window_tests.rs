
/// #609: 直近ウィンドウは索引の進み具合ではなく**予算**で決まる。
///
/// 索引ビルダーが現ターンとほぼ同時刻まで進むと `indexed_boundary`（topic が覆う最後の
/// log_id）がライブ末尾に張り付き、旧実装では `id > boundary` がほぼ空になって下限
/// フォールバック（`RECENT_MIN_LOGS`）へ縮退し、予算が大量に余っているのに直近 raw が
/// 十数件しか載らなかった。ここでは**索引が全ログを覆った（末尾に張り付いた）状態**を
/// 作り、それでも直近ウィンドウが予算ぶんの raw を載せることを固定する。
#[cfg(test)]
mod budget_driven_recent_window_tests {
    use super::{
        build_conversation_string, build_recent_window, estimate_tokens, retain_conversation_logs,
    };

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    /// 任意の log_type / speaker で 1 行積む（retain が落とす行の混入や #284 の作り込み用）。
    fn insert_raw(
        conn: &rusqlite::Connection,
        log_type: &str,
        speaker: Option<&str>,
        content: &str,
    ) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker.map(|s| s.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// `n` 件の（エージェント自身の）発話を積む。id は 1..=n（autoincrement）。
    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: AGENT.to_string(),
                    session_id: SESSION.to_string(),
                    log_type: "speech".to_string(),
                    content: format!(
                        "recent log line {i} about the release plan and the follow-up work"
                    ),
                    speaker_id: Some(AGENT.to_string()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    /// 索引が全ログ（1..=end）を覆う topic を 1 件置く。`end_log_id` を最終 log_id に
    /// することで `indexed_boundary` をライブ末尾へ張り付かせる（＝旧実装の縮退条件）。
    fn seed_topic_covering_all(conn: &rusqlite::Connection, end_log_id: i64) {
        opencrab_db::queries::insert_index_node(
            conn,
            &opencrab_db::queries::IndexNodeRow {
                id: "t-all".to_string(),
                agent_id: AGENT.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "作業ログ".to_string(),
                summary: "リリース準備の一連の作業をまとめた要約。".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(end_log_id),
                source_session_id: Some(SESSION.to_string()),
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-08-14T00:00:00Z".to_string(),
                updated_at: "2026-08-14T00:00:00Z".to_string(),
                short_id: Some("t-all".to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    /// 索引が末尾に張り付いていても、直近ウィンドウは予算ぶんの raw を載せる。
    ///
    /// 旧実装なら `id > indexed_boundary` が空 → `RECENT_MIN_LOGS`(=10) 件へ縮退し、
    /// ここが十数件で頭打ちになる。予算駆動なら残り予算いっぱいまで詰まる。
    #[test]
    fn recent_window_fills_budget_even_when_index_reaches_live_tail() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 200;
        seed_logs(&conn, N);
        // 索引を全ログの末尾へ張り付かせる（id は 1..=N）。
        seed_topic_covering_all(&conn, N as i64);

        // 全文（~200 件）は予算を超えてコンパクションへ入るが、残り予算は十数件どころか
        // 数十件を載せられるだけ余っている。
        const BUDGET: usize = 2_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();

        assert!(
            out.contains("[old_history_summary]"),
            "二水位圧縮の印が無い: {out}"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない: {out}"
        );
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "出力が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
        );
    }

    /// 取得件数の固定上限が無い（#610 レビュー①）。旧 `RECENT_LOG_FETCH_LIMIT=500` の頭打ちが消えた。
    ///
    /// `build_recent_window` を直接叩き、500 を超える件数を積んで巨大予算を渡す。予算で 1 件も
    /// 落ちないので、**一番古い行（末尾から N 件目）まで**出力に載る。旧実装は末尾 500 件しか
    /// 取得しなかったので index 0（＝末尾から 700 件目）は原理的に載らなかった。
    #[test]
    fn recent_window_has_no_fixed_fetch_cap() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 700; // > 旧 RECENT_LOG_FETCH_LIMIT (500)
        seed_logs(&conn, N);
        // 予算で 1 件も落とさない（全件が fit に載る）。
        let out = build_recent_window(&conn, SESSION, AGENT, usize::MAX);
        assert!(
            out.contains("recent log line 0"),
            "末尾 500 件より深い最古の行が載っていない（取得上限が残っている疑い）"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない"
        );
    }

    /// **pre-fit 欠落ゼロ**（#610 レビュー①の核心）。`fit` に渡る入力が
    /// `retain_conversation_logs(全件)` と一致する ——「fit に渡る前に対象外になって黙って消える行」
    /// がゼロであること。#609 が本当に守りたいのはこれで、省略マーカーの**文言には依存しない**。
    ///
    /// evaluation（#291）と heartbeat scaffolding（#501）は retain が落とすが、それ以外の全ログは
    /// fit に渡る。巨大予算で fit が 1 件も落とさない状態にし、**retain 後の全件が出力に現れ、
    /// retain が落とす行は現れない**ことを固定する。
    #[test]
    fn recent_window_feeds_every_retained_log_to_fit() {
        let conn = opencrab_db::init_memory().unwrap();
        // 会話に載る行（speech）を積む。
        seed_logs(&conn, 30);
        // retain が落とす行を途中に混ぜる: evaluation（#291）と heartbeat scaffolding（#501）。
        insert_raw(
            &conn,
            "evaluation",
            Some("evaluator"),
            "採点結果は却下マーカー",
        );
        insert_raw(
            &conn,
            "system",
            Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID),
            "巡回指示マーカー",
        );
        // 落とす行の後ろにも会話が続く形にする。
        for i in 30..35 {
            insert_raw(
                &conn,
                "speech",
                Some(AGENT),
                &format!("recent log line {i} tail"),
            );
        }

        // 期待値: retain 後の全件。fit に渡る入力がこれと一致することを固定する。
        let all = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
        let retained = retain_conversation_logs(all);
        assert_eq!(retained.len(), 35, "retain の残存件数が想定と違う");

        // 巨大予算 → fit は 1 件も落とさない。
        let out = build_recent_window(&conn, SESSION, AGENT, usize::MAX);

        // retain が残す行はすべて出力に現れる（pre-fit で 1 件も落ちない）。
        for log in &retained {
            assert!(
                out.contains(&log.content),
                "retain 後の行が fit に渡っていない（pre-fit 欠落）: {}",
                log.content
            );
        }
        // retain が落とす行は現れない。
        assert!(
            !out.contains("採点結果は却下マーカー"),
            "evaluation が混ざった: {out}"
        );
        assert!(
            !out.contains("巡回指示マーカー"),
            "heartbeat scaffolding が混ざった: {out}"
        );
    }

    /// #284 の維持（`merge_recent_user_speeches` 削除後）。大量のツール往復で古いユーザー発言が
    /// 末尾から押し出されても、**一番新しいユーザー発言**は小予算でも必ず載る。
    ///
    /// 全ログを fit へ渡すので、混ぜ戻し（旧 merge）が無くても直近ユーザー発言は入力に含まれ、
    /// `fit_logs_to_budget` の必須枠（`RECENT_MIN_USER_SPEECHES` / 飛び地 A′）が拾う。
    #[test]
    fn newest_user_speech_survives_tool_flood_after_merge_removal() {
        let conn = opencrab_db::init_memory().unwrap();
        // 一番古い位置に置くユーザー発言（これが「今の指示」で、末尾からは押し出される）。
        insert_raw(&conn, "speech", Some("owner"), "この指示は消えてはいけない");
        // 末尾を埋める巨大なツール往復（ユーザー発言を連続区間の外へ押し出す）。
        for i in 0..40 {
            insert_raw(
                &conn,
                "tool_result",
                Some(AGENT),
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
        }
        // topic を 1 件置いてコンパクション経路（切り詰めではない方）へ入れる。
        seed_topic_covering_all(&conn, 100);

        let out = build_conversation_string(&conn, SESSION, AGENT, 400).unwrap();
        assert!(
            out.contains("この指示は消えてはいけない"),
            "一番新しいユーザー発言が押し出された（#284 が壊れた）: {out}"
        );
    }

    /// topic 無しフォールバックも圧縮パスと同じ `build_recent_window` を通り、予算駆動になる（#610 レビュー②）。
    ///
    /// topic を 1 件も置かず切り詰めフォールバックへ落とす。予算はコンパクションを起こすが、
    /// 下限（`RECENT_MIN_LOGS`=10）ではなく予算ぶん（数十件）が載る。取得上限が無いこと自体は
    /// 共有関数を直接叩く [`recent_window_has_no_fixed_fetch_cap`] で固定済み。
    #[test]
    fn topic_less_fallback_routes_through_budget_driven_window() {
        let conn = opencrab_db::init_memory().unwrap();
        const N: usize = 300;
        seed_logs(&conn, N); // topic は置かない → 切り詰めフォールバック
        const BUDGET: usize = 2_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        assert!(
            !out.contains("[Past context summary"),
            "廃止した topic 要約が出ている: {out}"
        );
        assert!(
            out.contains("[old_history_summary]"),
            "コンパクションが起きていない（マーカー無し）: {out}"
        );
        assert!(
            out.contains(&format!("recent log line {}", N - 1)),
            "最新行が載っていない: {out}"
        );
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "出力が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
        );
    }
}
