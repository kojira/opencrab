
#[cfg(test)]
mod memory_index_section_injection_tests {
    use super::{build_conversation_string, build_conversation_string_with_memory_index};
    use crate::context_budget::{
        apply_line_items, compute_water_levels, ContextBudgetPolicy, MeasuredLineItems,
        MemoryIndexDecision, MemoryIndexOmitReason,
    };
    use crate::tokens::estimate_tokens;

    fn mk_node(
        id: &str,
        node_type: &str,
        parent: Option<&str>,
        title: &str,
        source_session_id: Option<&str>,
        date_from: Option<&str>,
    ) -> opencrab_db::queries::IndexNodeRow {
        opencrab_db::queries::IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: parent.map(String::from),
            node_type: node_type.to_string(),
            source_type: "session_log".to_string(),
            title: title.to_string(),
            summary: format!("{title} の要約"),
            start_log_id: None,
            end_log_id: None,
            source_session_id: source_session_id.map(String::from),
            date_from: date_from.map(String::from),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            short_id: Some(id.to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn seed_index(conn: &rusqlite::Connection) {
        use opencrab_db::queries::*;
        insert_index_node(conn, &mk_node("r1", "root", None, "root", None, None)).unwrap();
        insert_index_node(
            conn,
            &mk_node("pmay", "period", Some("r1"), "2026-05", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node("pjun", "period", Some("r1"), "2026-06", None, None),
        )
        .unwrap();
        update_period_rollup(conn, "pmay", "5月は逆引き辞書を設計した。", "[\"FTS\"]").unwrap();
        insert_index_node(
            conn,
            &mk_node("s1", "session", Some("pjun"), "S", None, None),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t-other",
                "topic",
                Some("s1"),
                "他セッション話題",
                Some("other-sess"),
                Some("2026-06-10"),
            ),
        )
        .unwrap();
        insert_index_node(
            conn,
            &mk_node(
                "t-cur",
                "topic",
                Some("s1"),
                "現セッション話題",
                Some("cur-sess"),
                Some("2026-06-11"),
            ),
        )
        .unwrap();
    }

    fn seed_logs(conn: &rusqlite::Connection, n: usize) {
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "a1".to_string(),
                    session_id: "cur-sess".to_string(),
                    log_type: "speech".to_string(),
                    content: format!("メッセージ {i} の内容がここに入る。{}", "詳細".repeat(40)),
                    speaker_id: Some("a1".to_string()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn injects_memory_index_exactly_once_under_budget() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let out = build_conversation_string(&conn, "cur-sess", "a1", 100_000).unwrap();
        assert_eq!(out.matches("[Memory Index]").count(), 1);
        // 月次要約が会話履歴に載る（中心要件）
        assert!(out.contains("5月は逆引き辞書を設計した。"));
        // 現在月 topic: 他セッションのみ
        assert!(out.contains("[t-other]"));
        assert!(!out.contains("[t-cur]"));
        // 予算内なので通常の全文会話が続く
        assert!(out.contains("メッセージ 2 の内容"));
    }

    #[test]
    fn no_index_means_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_logs(&conn, 2);
        let out = build_conversation_string(&conn, "cur-sess", "a1", 100_000).unwrap();
        assert!(!out.contains("[Memory Index]"));
    }

    #[test]
    fn tiny_budget_skips_section() {
        // 残予算に収まらなければ丸ごと省略（部分切り詰めなし）。判定は apply_line_items。
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let section = crate::memory_index::build_memory_index_section(&conn, "a1", "cur-sess")
            .unwrap()
            .expect("index section");
        let cost = estimate_tokens(&section);
        let policy = ContextBudgetPolicy {
            absolute_cap_a: 100,
            memory_index_token_cap: 4_000,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(10_000, 50, &policy).unwrap();
        // input_high=100, mandatory=80, remaining=20。MI は残予算を超えて省略。
        assert!(
            cost > 20,
            "fixture MI should exceed remaining 20, got {cost}"
        );
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 10,
                functions: 10,
                memory_index: cost,
                memory_index_entry_count: 3,
                conversation: 0,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsRemainingBudget
            }
        );
        let include = matches!(env.memory_index_decision, MemoryIndexDecision::Inject);
        // MI 省略が主題。会話車線の高水位は envelope の 20 tok ではなく十分確保する。
        let out =
            build_conversation_string_with_memory_index(&conn, "cur-sess", "a1", 100_000, include)
                .unwrap();
        assert!(!out.contains("[Memory Index]"));
        assert!(!out.contains("5月は逆引き辞書を設計した。"));
    }

    #[test]
    fn dedicated_cap_omits_memory_index_entirely() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        seed_logs(&conn, 3);
        let section = crate::memory_index::build_memory_index_section(&conn, "a1", "cur-sess")
            .unwrap()
            .expect("index section");
        let cost = estimate_tokens(&section);
        let policy = ContextBudgetPolicy {
            memory_index_token_cap: 1,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(200_000, 4_096, &policy).unwrap();
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 10,
                functions: 10,
                memory_index: cost,
                memory_index_entry_count: 3,
                conversation: 0,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsDedicatedCap
            }
        );
        let include = matches!(env.memory_index_decision, MemoryIndexDecision::Inject);
        let out = build_conversation_string_with_memory_index(
            &conn,
            "cur-sess",
            "a1",
            env.conversation_high,
            include,
        )
        .unwrap();
        assert!(!out.contains("[Memory Index]"));
        assert!(!out.contains("5月は逆引き辞書を設計した。"));
        assert!(out.contains("メッセージ 2 の内容"));
    }

    #[test]
    fn compaction_path_keeps_short_id_sets_disjoint() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_index(&conn);
        // 現セッション topic に log 範囲を持たせ、コンパクション時の
        // [Past context summary] に出るようにする
        seed_logs(&conn, 40);
        conn.execute(
            "UPDATE memory_index_nodes SET start_log_id = 1, end_log_id = 20 WHERE id = 't-cur'",
            [],
        )
        .unwrap();
        // Memory Index は専用 cap 判定（apply_line_items）済みとして通す。
        // 826-B で現行セッション topic の [Past context summary] は廃止。
        let out = build_conversation_string(&conn, "cur-sess", "a1", 900).unwrap();
        assert_eq!(out.matches("[Memory Index]").count(), 1);
        assert!(!out.contains("[Past context summary"));
        // 他セッション topic は Memory Index 側のみ。現セッション topic はどちらにも出ない。
        assert!(!out.contains("[t-cur]"));
        assert_eq!(out.matches("[t-other]").count(), 1);
        let mi_pos = out.find("[Memory Index]").unwrap();
        let tother_pos = out.find("[t-other]").unwrap();
        assert!(tother_pos > mi_pos);
    }
}
