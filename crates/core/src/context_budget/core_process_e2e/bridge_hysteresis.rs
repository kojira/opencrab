/// D-1077: 現セッションのトピック要約（bridge）を置いた状態で、本番と同じ開始組立
/// （`build_conversation_string_with_waters`）と終了圧縮（`load_bridge_lines` + `finish_turn`）を回す。
fn seed_bridge_topics(conn: &rusqlite::Connection, logs_per_topic: i64) -> usize {
    let logs = opencrab_db::queries::list_session_logs_by_session(conn, SESSION).unwrap();
    let max_id = logs.iter().filter_map(|l| l.id).max().unwrap();
    let mut total = 0;
    let mut start = 1;
    let mut n = 0;
    while start <= max_id {
        let end = (start + logs_per_topic - 1).min(max_id);
        let summary = format!("topic {n} summary {}", "detail words here ".repeat(100));
        total += estimate_tokens(&summary);
        opencrab_db::queries::insert_index_node(
            conn,
            &opencrab_db::queries::IndexNodeRow {
                id: format!("topic-{n}"),
                agent_id: AGENT.into(),
                parent_id: None,
                node_type: "topic".into(),
                source_type: "session_log".into(),
                title: format!("T{n}"),
                summary,
                start_log_id: Some(start),
                end_log_id: Some(end),
                source_session_id: Some(SESSION.into()),
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-10-01T00:00:00Z".into(),
                updated_at: "2026-10-01T00:00:00Z".into(),
                short_id: Some(format!("t{n}")),
                keywords_json: "[]".into(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
        start = end + 1;
        n += 1;
    }
    total
}

/// 本番 `persist_turn_end_snapshot` と同じ手順（server/src/process/budget.rs）。
fn production_turn_end(conn: &rusqlite::Connection) -> super::compact::CompactOutcome {
    let assembled = assemble_from_snapshot(conn, SESSION, AGENT).unwrap();
    let bridge = super::bridge::load_bridge_lines(conn, AGENT, SESSION).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW).with_bridge(bridge);
    gov.finish_turn(
        conn,
        SESSION,
        &assembled.items,
        assembled.through_log_id,
        &assembled.text,
    )
    .unwrap()
}

/// (d') トピック要約がある会話でも、圧縮後は全体が低水位以内に収まり、高水位を越えるまで
/// 再発火しない。その間の開始組立は前回の本文に追記するだけ（prefix が byte 一致）なので、
/// プロンプトキャッシュが前回分まで読める。
#[test]
fn d_hysteresis_with_topic_bridge_is_append_only_until_high() {
    let conn = opencrab_db::init_memory().unwrap();
    // 本番同様、逐語の手前に小さなトピックが多数ある長い会話にする。
    for i in 0..150 {
        insert_speech(&conn, "owner", &format!("early-{i}"));
    }
    seed_over_high(&conn);
    let bridge_total = seed_bridge_topics(&conn, 1);
    assert!(
        bridge_total > HIGH,
        "前提: bridge 全量 {bridge_total} が高水位より大きい（高水位まで詰めれば満杯になる）"
    );
    let _ = take_governor_events();

    let first = production_turn_end(&conn);
    assert!(first.fired, "最初の終了圧縮で発火する");
    assert!(
        first.text.contains(super::bridge::BRIDGE_HEADER),
        "前提: bridge が入る: {}",
        &first.text[..first.text.len().min(400)]
    );
    assert!(
        first.after_tokens <= LOW,
        "圧縮後の全体（逐語＋bridge）が低水位 {LOW} 以内: after={}",
        first.after_tokens
    );

    let mut previous = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
        .expect("圧縮直後の開始組立");
    for n in 0..5 {
        insert_speech(&conn, "owner", &format!("post-{n} {}", "small talk words ".repeat(60)));
        let started =
            build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
                .expect("開始組立");
        let stable = previous
            .strip_suffix(crate::conversation::CONVERSATION_HISTORY_END)
            .expect("履歴の閉じタグで終わる");
        assert!(
            started.starts_with(stable),
            "投稿 {n}: 開始組立が前回の本文への追記になっていない（prefix 不一致）"
        );
        insert_speech(&conn, AGENT, &format!("reply-{n}"));
        let end = production_turn_end(&conn);
        assert!(!end.fired, "投稿 {n} で再発火: before={}", end.before_tokens);
        previous = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
            .expect("開始組立");
    }

    let fires = compact_fired_phases(&take_governor_events());
    assert_eq!(
        fires,
        vec![CompactPhase::TurnEnd],
        "bridge があるとヒステリシスが効かず毎回発火した: {fires:?}"
    );
}
