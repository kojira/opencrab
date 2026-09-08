
/// `[Past context summary]` の予算上限（#406）。
///
/// 事故当時、このセクションには上限が無く、topic 2,446 件・要約 248,340 文字が全件
/// 連結され、1 ハートビートの入力が 284,486 トークンになっていた。ここで固定するのは
/// **上限が効くこと**と**切り詰めの向き（新しい方が残る）**の 2 点。
#[cfg(test)]
mod past_summary_budget_tests {
    use super::{
        build_conversation_string, build_past_context_summary_section, estimate_tokens,
        PAST_SUMMARY_BUDGET_DEN, PAST_SUMMARY_BUDGET_NUM,
    };

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    fn insert_log(conn: &rusqlite::Connection, session_id: &str, speaker: &str, content: String) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content,
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 現セッションの topic を `n` 件置く。`start_log_id` は昇順（＝供給元の
    /// `ORDER BY start_log_id ASC` で **TOPIC-000 が最古**になる）。
    fn seed_topics(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_index_node(
                conn,
                &opencrab_db::queries::IndexNodeRow {
                    id: format!("t{i:03}"),
                    agent_id: AGENT.to_string(),
                    parent_id: None,
                    node_type: "topic".to_string(),
                    source_type: "session_log".to_string(),
                    title: format!("TOPIC-{i:03}"),
                    summary: format!(
                        "summary body for topic {i:03} {}",
                        "padding words to make the line非自明な長さ ".repeat(3)
                    ),
                    start_log_id: Some(i as i64 + 1),
                    end_log_id: None,
                    source_session_id: Some(SESSION.to_string()),
                    date_from: None,
                    date_to: None,
                    depth: 0,
                    child_count: 0,
                    token_count: 0,
                    created_at: "2026-07-01T00:00:00Z".to_string(),
                    updated_at: "2026-07-01T00:00:00Z".to_string(),
                    short_id: Some(format!("t{i:03}")),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                },
            )
            .unwrap();
        }
    }

    /// 会話本文だけで予算を超えるだけのログを積む（＝コンパクション経路へ入れる）。
    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            insert_log(
                conn,
                SESSION,
                AGENT,
                format!("log line {i} about the release plan and the follow-up work"),
            );
        }
    }

    fn topic_rows(conn: &rusqlite::Connection) -> Vec<opencrab_db::queries::IndexNodeRow> {
        opencrab_db::queries::get_topic_nodes_for_session(conn, AGENT, SESSION).unwrap()
    }

    fn built_summary(conn: &rusqlite::Connection, budget: usize) -> String {
        build_past_context_summary_section(&topic_rows(conn), budget)
    }

    /// topic が数千件あっても、セクションは予算の 30% を超えない。
    ///
    /// 826-B で本番組立からは外したので、ヘルパーを直接叩く。
    #[test]
    fn past_summary_stays_within_thirty_percent_of_the_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 2_000);

        const BUDGET: usize = 4_000;
        let cap = BUDGET / PAST_SUMMARY_BUDGET_DEN * PAST_SUMMARY_BUDGET_NUM;
        let section = built_summary(&conn, cap);
        let used = estimate_tokens(&section);
        assert!(
            used <= cap,
            "[Past context summary] が予算の 30% ({cap}) を超えた: {used} トークン"
        );
        assert!(
            !section.contains("TOPIC-000"),
            "2,000 件が全件載っている（切り詰めが起きていない）"
        );
    }

    /// **切り詰めの向き**: 新しい topic が残り、古い topic から落ちる。
    ///
    /// 供給元のクエリは古い順なので、素直に前から詰めると逆になる。詰める向きを
    /// 反転させたらこのテストが落ちること。表示順が時系列（古い→新しい）へ戻って
    /// いることも同時に見る。
    #[test]
    fn past_summary_keeps_the_newest_topics_and_drops_the_oldest() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 100);

        let section = built_summary(&conn, 1_200);
        assert!(
            section.contains("TOPIC-099"),
            "最新の topic が落ちている: {section}"
        );
        assert!(
            !section.contains("TOPIC-000"),
            "最古の topic が残っている（切り詰めの向きが逆）: {section}"
        );
        let newest = section.find("TOPIC-099").unwrap();
        let one_before = section
            .find("TOPIC-098")
            .expect("直前の topic まで落ちている（残す件数が想定より少ない）");
        assert!(
            one_before < newest,
            "表示順が時系列に戻っていない（新しい方が先に出ている）: {section}"
        );
    }

    /// 落としたら黙らない: 件数と引き出し方を本人へ伝える。
    #[test]
    fn past_summary_reports_how_many_were_omitted() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 100);

        let section = built_summary(&conn, 1_200);
        assert!(
            section.contains("older topic summaries were omitted"),
            "落としたことが伝わっていない: {section}"
        );
        assert!(
            section.contains("retrieve_memory_nodes"),
            "引き出し方が書かれていない: {section}"
        );
        // 残った件数と落とした件数の合計は 100。告知の件数が実態と食い違わないこと。
        let kept = (0..100)
            .filter(|i| section.contains(&format!("TOPIC-{i:03}")))
            .count();
        assert!(
            section.contains(&format!("{} older topic summaries", 100 - kept)),
            "告知の件数が残存件数と合っていない（残 {kept} 件）: {section}"
        );
    }

    /// #408: 全 topic が予算内に収まる（早期 return）経路を固定する。既存テストは
    /// いずれも切り詰めが起きる seed（topic 100 件 / 予算 4,000 など）なので、
    /// `build_past_context_summary_section` の `if dropped == 0` を潰す変異（→ `if false`）
    /// を入れても素通りしていた。
    ///
    /// コンパクション（＝ `[Past context summary]` の構築）は**全文が予算を超えたときだけ**
    /// 走る（`build_conversation_inner` の全文フィット早期 return）。そこで**ログは大量**に
    /// 置いて予算超過でコンパクションを起こしつつ、**topic は少数**（3 件）に絞って 30% 枠
    /// （4,000 × 3/10 = 1,200 トークン）に全件が収まる状況を作る。ここで
    /// (1) 切り詰めの告知が出ない (2) 全 topic が出力に含まれる ことを固定する。
    ///
    /// 変異を入れると `dropped == 0` のまま告知構築へ落ち、`past_summary_omitted_notice(0)`
    /// （"0 older topic summaries were omitted ..."）が混入してここで落ちる。
    /// `topics.is_empty()` の fallback 経路（`build_recent_window`）とは別物で、
    /// こちらは「topic はあるが全部入る」ケース。
    #[test]
    fn past_summary_emits_no_notice_when_all_topics_fit() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_topics(&conn, 3);

        let section = built_summary(&conn, 4_000);

        // 全 topic が [Past context summary] に出る。
        for i in 0..3 {
            assert!(
                section.contains(&format!("TOPIC-{i:03}")),
                "全 topic が出力に含まれるべき (TOPIC-{i:03}): {section}"
            );
        }
        // 全件が収まるので切り詰めの告知は一切出ない（早期 return 経路）。
        assert!(
            !section.contains("were omitted"),
            "全件が収まるのに切り詰めの告知が出ている（早期 return が壊れている）: {section}"
        );
    }

    /// 要約が予算を食い潰さないので、直近会話の枠が残る（事故当時はここが 0 だった）。
    ///
    /// #500 の位置づけ: これは**コンパクションが機能していることの回復ガード**であって、
    /// heartbeat 障害の再発防止ではない。障害時の会話は予算（525,000）に収まっていたのに、
    /// その予算自体がバックエンドの実上限（371,678）を超えていた。**「予算 ≤ バックエンド
    /// 実上限」の天井はまだコードに無く #535 の管轄**。この `<= BUDGET` assert が意味を持つのは
    /// budget が正しく設定されている前提でのみ。
    #[test]
    fn recent_conversation_keeps_its_share_when_topics_are_huge() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 400);
        seed_topics(&conn, 2_000);

        const BUDGET: usize = 4_000;
        let out = build_conversation_string(&conn, SESSION, AGENT, BUDGET).unwrap();
        assert!(
            out.contains("[old_history_summary]"),
            "二水位圧縮の印が無い: {out}"
        );
        assert!(out.contains("log line 399"), "直近ログが落ちている: {out}");
        assert!(
            estimate_tokens(&out) <= BUDGET,
            "プロンプト全体が予算 {BUDGET} を超えた: {} トークン",
            estimate_tokens(&out)
        );
    }

    /// 予算が極小でも panic しない（0 除算・スライス外・オーバーフロー）。
    ///
    /// **panic しないことだけを見る。** 極小予算では直近下限（`RECENT_MIN_LOGS`）が予算を
    /// 割って出力が予算を超えうるが、それはここでは固定しない（超過そのものは #536）。
    #[test]
    fn tiny_budget_does_not_panic() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 20);
        seed_topics(&conn, 50);

        for budget in 0..=3 {
            match build_conversation_string(&conn, SESSION, AGENT, budget) {
                Ok(out) => {
                    assert!(!out.is_empty(), "budget={budget} で空文字になった");
                }
                Err(e) => {
                    let msg = format!("{e}");
                    assert!(
                        msg.contains(crate::context_budget::CONTEXT_BUDGET_EXHAUSTED),
                        "budget={budget} は exhausted 以外: {msg}"
                    );
                }
            }
        }
    }
}
