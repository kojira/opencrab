
/// #291: 既に DB にある `evaluation` 行を会話文字列へ復元しない。
///
/// 対話ターンからの evaluator 呼び出しは撤去したが、過去に記録された行は本番 DB に
/// 残る。読み出し側でも落とさないと、次のターンで採点結果と「次ターンでギャップを
/// 埋めろ」という指示文が復活し、直前のユーザー発言と同じ土俵に並んでしまう。
/// 全文経路・コンパクション経路・切り詰め経路のすべてで落ちることを確かめる。
#[cfg(test)]
mod evaluation_not_in_conversation_tests {
    use super::build_conversation_string;

    const AGENT: &str = "a1";
    const SESSION: &str = "s1";

    /// 事故当時と同じ形の evaluation 行（採点 + 指示文）。
    const EVAL_CONTENT: &str = "score 0.05/0.70 (not satisfied) — 証拠が無い\ngaps:\n- 未検証\nAddress these gaps in your next turn (claims without evidence in the trace do not count).";

    fn insert(conn: &rusqlite::Connection, log_type: &str, speaker: &str, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    fn seed(conn: &rusqlite::Connection) {
        insert(
            conn,
            "speech",
            "owner",
            "既存フォローはわたしだけなのでは？",
        );
        insert(conn, "evaluation", "evaluator", EVAL_CONTENT);
        insert(conn, "speech", AGENT, "確認します。");
    }

    #[test]
    fn evaluation_rows_are_dropped_from_the_full_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);

        let out = build_conversation_string(&conn, SESSION, AGENT, 100_000).unwrap();
        assert!(
            !out.contains("[evaluation]"),
            "evaluation 行が会話に復元されている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "採点の指示文が会話に復元されている: {out}"
        );
        // 人間の発言は残る（除外が効きすぎていないこと）。
        assert!(out.contains("既存フォローはわたしだけなのでは？"), "{out}");
        assert!(out.contains("確認します。"), "{out}");
    }

    /// コンパクション経路（topic 要約あり）でも落ちること。
    #[test]
    fn evaluation_rows_are_dropped_from_the_compacted_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        for i in 0..30 {
            insert(
                &conn,
                "tool_result",
                AGENT,
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
            insert(&conn, "evaluation", "evaluator", EVAL_CONTENT);
        }
        // topic 要約を 1 件置いてコンパクション経路（切り詰めではない方）へ入れる。
        opencrab_db::queries::insert_index_node(
            &conn,
            &opencrab_db::queries::IndexNodeRow {
                id: "t1".to_string(),
                agent_id: AGENT.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "作業ログ".to_string(),
                summary: "フォロー作業を進めていた。".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: Some(SESSION.to_string()),
                date_from: Some("2026-07-01".to_string()),
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-07-01T00:00:00Z".to_string(),
                updated_at: "2026-07-01T00:00:00Z".to_string(),
                short_id: Some("t1".to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();

        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            !out.contains("[evaluation]"),
            "コンパクション経路で evaluation 行が残っている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "コンパクション経路で採点の指示文が残っている: {out}"
        );
    }

    #[test]
    fn evaluation_rows_are_dropped_from_the_truncated_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        seed(&conn);
        // 全文が予算に収まらない状態にして切り詰め経路へ落とす。
        for i in 0..30 {
            insert(
                &conn,
                "tool_result",
                AGENT,
                &format!("結果 {i}: {}", "x".repeat(400)),
            );
            insert(&conn, "evaluation", "evaluator", EVAL_CONTENT);
        }

        let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
        assert!(
            !out.contains("[evaluation]"),
            "切り詰め経路で evaluation 行が残っている: {out}"
        );
        assert!(
            !out.contains("Address these gaps in your next turn"),
            "切り詰め経路で採点の指示文が残っている: {out}"
        );
    }
}
