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
    let (high, low) =
        crate::conversation::conversation_history_waters(conn, SESSION, AGENT, HIGH, LOW);
    let mut gov = TurnGovernor::new(high, low).with_bridge(bridge);
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

/// 会話車線に計上される前置（台帳）と、計上されない [Memory Index] を置く。
fn seed_ledger_and_memory_index(conn: &rusqlite::Connection) {
    let task = opencrab_db::queries::insert_task_ledger(
        conn,
        AGENT,
        SESSION,
        &"ship the long-running feature ".repeat(120),
        Some(&"all acceptance checks pass ".repeat(140)),
    )
    .unwrap();
    for n in 0..10 {
        opencrab_db::queries::insert_task_progress(
            conn,
            task,
            "progress",
            &format!("step {n} {}", "progress detail ".repeat(30)),
        )
        .unwrap();
    }
    opencrab_db::queries::insert_index_node(
        conn,
        &opencrab_db::queries::IndexNodeRow {
            id: "period-2026-09".into(),
            agent_id: AGENT.into(),
            parent_id: None,
            node_type: "period".into(),
            source_type: "session_log".into(),
            title: "2026-09".into(),
            summary: "september summary".into(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-01T00:00:00Z".into(),
            short_id: Some("p9".into()),
            keywords_json: "[]".into(),
            summary_refreshed_at: Some("2026-10-01T00:00:00Z".into()),
        },
    )
    .unwrap();
}

/// 開始組立の会話本文（履歴タグの内側）。終了時の `assembled.text` と同じ量。
fn history_inner(started: &str) -> &str {
    let open = format!("{}\n", crate::conversation::CONVERSATION_HISTORY_START);
    let close = format!("\n{}", crate::conversation::CONVERSATION_HISTORY_END);
    let from = started.find(&open).expect("履歴の開始タグ") + open.len();
    let to = started.rfind(&close).expect("履歴の閉じタグ");
    &started[from..to]
}

/// (d'') 台帳（会話車線に計上）と [Memory Index] がある会話で、組立本文が
/// 「高水位 − 前置」と高水位の間の帯に入っても、開始組立はターン終了が書いた
/// スナップショット＋差分をそのまま使い、刈り直さない。開始と終了が同じ量（履歴本文）を
/// 同じ水位で比べるので、終了時の判定が開始時に覆らない。prefix は byte 一致で伸びる。
#[test]
fn d_hysteresis_band_below_high_does_not_refire_at_turn_start() {
    let conn = opencrab_db::init_memory().unwrap();
    for i in 0..150 {
        insert_speech(&conn, "owner", &format!("early-{i}"));
    }
    seed_over_high(&conn);
    seed_bridge_topics(&conn, 1);
    seed_ledger_and_memory_index(&conn);
    let ledger = crate::task_ledger::build_ledger_section(&conn, AGENT, SESSION)
        .unwrap()
        .expect("前提: 台帳セクションがある");
    let prefix_cost = estimate_tokens(&ledger);
    assert!(prefix_cost > 1_500, "前提: 台帳 {prefix_cost} tokens");
    let _ = take_governor_events();

    let first = production_turn_end(&conn);
    assert!(first.fired, "最初の終了圧縮で発火する");

    // 組立本文を帯（高水位 − 前置, 高水位] まで伸ばしてからターンを終える。
    let current = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap().tokens;
    let unit = "growth words here ";
    let per_unit = estimate_tokens(&unit.repeat(100)) / 100;
    let target = HIGH - prefix_cost + 300;
    insert_speech(&conn, "owner", &unit.repeat((target - current) / per_unit));
    insert_speech(&conn, AGENT, "noted");
    let band = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap().tokens;
    assert!(
        band > HIGH - prefix_cost && band <= HIGH,
        "前提: 本文 {band} が帯 ({}, {HIGH}] にある",
        HIGH - prefix_cost
    );
    production_turn_end(&conn);
    let _ = take_governor_events();

    let mut previous = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, true)
        .expect("開始組立");
    assert!(previous.contains("[Memory Index]"), "前提: MI が載る");
    assert!(previous.starts_with("[Task Ledger]"), "前提: 台帳が載る");
    for n in 0..4 {
        insert_speech(&conn, "owner", &format!("band-{n} {}", "short words ".repeat(20)));
        let started =
            build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, true)
                .expect("開始組立");
        let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
        assert!(
            history_inner(&started) == assembled.text,
            "投稿 {n}: 開始組立がスナップショット＋差分（本文 {}）を刈り直した",
            assembled.tokens
        );
        let stable = previous
            .strip_suffix(crate::conversation::CONVERSATION_HISTORY_END)
            .expect("履歴の閉じタグで終わる");
        assert!(
            started.starts_with(stable),
            "投稿 {n}: 開始組立が前回の本文への追記になっていない（prefix 不一致）"
        );
        insert_speech(&conn, AGENT, &format!("band-reply-{n}"));
        let end = production_turn_end(&conn);
        assert!(!end.fired, "投稿 {n} で終了時に再発火: before={}", end.before_tokens);
        previous = started;
    }

    let fires = compact_fired_phases(&take_governor_events());
    assert!(fires.is_empty(), "帯の後の数ターンで圧縮が発火した: {fires:?}");
}
