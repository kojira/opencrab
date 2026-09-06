use super::super::*;
/// v33: sleep のメンテナンスラン（宣言 `sleep-declare-*` / 整理 `sleep-organize-*`）が
/// 生んだ生ログを消す（#393）。本体と FTS を**同一 rowid 集合**で消すこと（＝孤児を
/// 増やさない）、他の接頭辞のログを 1 行も巻き込まないこと、既存の孤児に触らないこと、
/// 運用記録（`llm_logs` / `agent_logs`）を消さないこと、冪等であることを固定する。
#[test]
fn v33_deletes_maintenance_run_logs_from_both_tables() {
    use crate::queries::{insert_session_log, SessionLogRow};
    let conn = crate::init_memory().expect("init");
    let agent = "a1";

    let mk = |session: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent.to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };

    // メンテナンスランの生ログ（消える）。
    let id_declare =
        insert_session_log(&conn, &mk("sleep-declare-a1-1700000000", "declare")).expect("declare");
    let id_organize = insert_session_log(&conn, &mk("sleep-organize-a1-1700000001", "organize"))
        .expect("organize");
    // 通常の会話・ハートビート等（残る）。`sleep` を含むが接頭辞ではない session_id や、
    // 接頭辞に見えて別物の `sleep-` も混ぜて、LIKE が広く効きすぎないことを確かめる。
    let keep = [
        ("discord-a1-100-200", "discord"),
        ("heartbeat-a1-100", "heartbeat"),
        ("subtask-42", "subtask"),
        ("nostr-a1", "nostr"),
        ("web-a1-conv", "web"),
        ("agent-msg-a1-u1", "rest"),
        ("discord-a1-sleep-declare-x", "not a maintenance run"),
    ];
    let keep_ids: Vec<i64> = keep
        .iter()
        .map(|(s, c)| insert_session_log(&conn, &mk(s, c)).expect("keep"))
        .collect();

    // 既存の孤児（本体に対応行が無い FTS 行）を 1 件仕込む。本番にも 208 行あり、
    // v33 がそれを増やしも減らしもしないことを固定する。
    conn.execute(
        "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
             VALUES (999999, 'orphan', ?1, 'discord-a1-orphan', 'speech')",
        [agent],
    )
    .expect("orphan");

    // 運用記録（#393 の追加受け入れ条件）。同じメンテナンスランの `llm_logs`
    // （session_id で引ける生プロンプト/生応答/tool_calls）と `agent_logs`
    // （context="sleep" の 1 ラン 1 行の要約）を仕込み、**v33 がこれらを消さない**ことを
    // 固定する。生ログから外すのは「記憶の材料としての扱い」だけで、何を行ったかの
    // 運用記録は残す。
    crate::queries::insert_llm_log(
        &conn,
        &crate::queries::LlmLogRow {
            id: "llm-1".to_string(),
            agent_id: agent.to_string(),
            session_id: Some("sleep-declare-a1-1700000000".to_string()),
            model: Some("m".to_string()),
            prompt: "p".to_string(),
            response: "r".to_string(),
            tool_calls: Some("[]".to_string()),
            latency_ms: Some(1),
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            error_code: None,
            error_body: None,
            requested_at: None,
            trigger_message_id: None,
            is_bot_iteration: false,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            created_at: "2026-08-05T00:00:00+00:00".to_string(),
        },
    )
    .expect("llm_log");
    crate::queries::insert_agent_log(
        &conn,
        &crate::queries::AgentLogRow {
            id: "audit-1".to_string(),
            agent_id: Some(agent.to_string()),
            level: "info".to_string(),
            context: "sleep".to_string(),
            message: r#"{"kind":"memory_declare"}"#.to_string(),
            created_at: None,
        },
    )
    .expect("agent_log");

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let orphans = |c: &Connection| -> i64 {
        c.query_row(
            "SELECT COUNT(*) FROM memory_sessions_fts f
                 LEFT JOIN memory_sessions m ON m.id = f.rowid WHERE m.id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let body_before = count("SELECT COUNT(*) FROM memory_sessions");
    let fts_before = count("SELECT COUNT(*) FROM memory_sessions_fts");
    let orphans_before = orphans(&conn);
    assert_eq!(orphans_before, 1, "孤児の仕込みが効いている");

    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 対象 2 行が本体からも FTS からも消えている。
    for id in [id_declare, id_organize] {
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
            )),
            0,
            "本体から消える"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
            )),
            0,
            "FTS からも同じ rowid で消える"
        );
    }
    // 他は 1 行も巻き込んでいない（本体・FTS とも）。
    for id in &keep_ids {
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
            )),
            1,
            "メンテナンスラン以外は残る"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
            )),
            1,
            "FTS 側も残る"
        );
    }
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_sessions"),
        body_before - 2
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_sessions_fts"),
        fts_before - 2
    );
    assert_eq!(orphans(&conn), orphans_before, "孤児は増えも減りもしない");

    // 運用記録は 1 行も消えていない。session_id が対象と同じ `sleep-declare-%` でも
    // `llm_logs` は対象外（消すのは memory_sessions と memory_sessions_fts だけ）。
    assert_eq!(
        count("SELECT COUNT(*) FROM llm_logs WHERE session_id LIKE 'sleep-declare-%'"),
        1,
        "llm_logs は消さない（何を行ったかの記録は残す）"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM agent_logs WHERE context = 'sleep'"),
        1,
        "agent_logs（sleep 監査）は消さない"
    );

    // 冪等: 版を 32 へ戻して再実行しても何も変わらない（対象 0 行）。
    let body_after = count("SELECT COUNT(*) FROM memory_sessions");
    let fts_after = count("SELECT COUNT(*) FROM memory_sessions_fts");
    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), body_after);
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions_fts"), fts_after);
    assert_eq!(orphans(&conn), orphans_before);
    assert_eq!(count("SELECT COUNT(*) FROM llm_logs"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM agent_logs"), 1);
}

/// v33（索引側 / #393 の 2 巡目レビュー指摘）: メンテナンスランのログから作られた
/// 索引ノードも消す。生ログだけ消すと「タイトルと要約はあるが中身を引くと空が返る記憶」が
/// 残るため（索引ビルドは本番で対象 id 帯を通過済み）。
///
/// 固定するもの:
/// - `source_session_id` がメンテナンスランを指す session / topic ノードが本体と
///   `memory_index_fts` の**同一 node_id 集合**で消えること
/// - 対象ノードの子孫も消えること（FTS 孤児を残さない）
/// - 空になる親（period）は**残す**こと
/// - 通常の会話由来のノードを 1 件も巻き込まないこと
/// - 宣言ユニットは「範囲が丸ごとメンテナンスラン由来」のものだけ消し、通常ログが
///   混じるユニット・範囲が空のユニットは残すこと
/// - `memory_category_members` の宙に浮く参照が消えること（残るノードの行は残ること）
/// - 冪等であること
#[test]
fn v33_deletes_maintenance_run_index_nodes_and_only_pure_maintenance_units() {
    use crate::queries::{insert_index_node, insert_session_log, IndexNodeRow, SessionLogRow};
    let conn = crate::init_memory().expect("init");
    let agent = "a1";

    let log = |session: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent.to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    // 生ログ: メンテナンスラン 2 行 → 通常 1 行 → メンテナンスラン 1 行 の順に入れて、
    // ユニットの範囲が「丸ごと」か「混在」かを id 範囲で作り分けられるようにする。
    let m1 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m1")).unwrap();
    let m2 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m2")).unwrap();
    let normal = insert_session_log(&conn, &log("discord-a1-1-2", "normal")).unwrap();
    let m3 = insert_session_log(&conn, &log("sleep-organize-a1-1", "m3")).unwrap();

    let node = |id: &str,
                parent: Option<&str>,
                node_type: &str,
                src_session: Option<&str>,
                range: Option<(i64, i64)>| IndexNodeRow {
        id: id.to_string(),
        agent_id: agent.to_string(),
        parent_id: parent.map(|s| s.to_string()),
        node_type: node_type.to_string(),
        source_type: if node_type == "unit" {
            "declared".to_string()
        } else {
            "session_log".to_string()
        },
        title: format!("title-{id}"),
        summary: format!("summary-{id}"),
        start_log_id: range.map(|r| r.0),
        end_log_id: range.map(|r| r.1),
        source_session_id: src_session.map(|s| s.to_string()),
        date_from: None,
        date_to: None,
        depth: 2,
        child_count: 0,
        token_count: 0,
        created_at: "2026-08-05T00:00:00+00:00".to_string(),
        updated_at: "2026-08-05T00:00:00+00:00".to_string(),
        short_id: None,
        keywords_json: "[]".to_string(),
        summary_refreshed_at: None,
    };

    // 親（period）と、その下のメンテナンス由来 session + その topic 子。
    insert_index_node(&conn, &node("period-1", None, "period", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "sess-m",
            Some("period-1"),
            "session",
            Some("sleep-declare-a1-1"),
            Some((m1, m2)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "topic-m",
            Some("sess-m"),
            "topic",
            Some("sleep-declare-a1-1"),
            Some((m1, m2)),
        ),
    )
    .unwrap();
    // 対象 session の子だが `source_session_id` が無い（再帰で一緒に消える = FTS 孤児を残さない）。
    insert_index_node(
        &conn,
        &node(
            "topic-m-nosrc",
            Some("sess-m"),
            "topic",
            None,
            Some((m1, m2)),
        ),
    )
    .unwrap();
    // 通常の会話由来（残る）。period-1 の子でもあるので、親が空にならない側も確かめられる。
    insert_index_node(
        &conn,
        &node(
            "sess-ok",
            Some("period-1"),
            "session",
            Some("discord-a1-1-2"),
            Some((normal, normal)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "topic-ok",
            Some("sess-ok"),
            "topic",
            Some("discord-a1-1-2"),
            Some((normal, normal)),
        ),
    )
    .unwrap();
    // 子が全て消えて空になる period（本番に 2 件あるケース）。索引ビルダの child_count
    // 再計算は「子を持つ親」しか書かないので、ここが 0 へ直らないと永久にずれたままになる。
    insert_index_node(&conn, &node("period-2", None, "period", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "sess-m2",
            Some("period-2"),
            "session",
            Some("sleep-organize-a1-1"),
            Some((m3, m3)),
        ),
    )
    .unwrap();
    // ユニット 3 種。範囲が丸ごとメンテナンス由来 / 通常ログ混在 / 範囲が空。
    // 宣言ユニットの根（本番の `declroot-{agent_id}`）も置き、子を 1 つ失う側を作る。
    insert_index_node(&conn, &node("declroot-1", None, "root", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-pure",
            Some("declroot-1"),
            "unit",
            None,
            Some((m1, m2)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-mixed",
            Some("declroot-1"),
            "unit",
            None,
            Some((m1, m3)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-empty",
            Some("declroot-1"),
            "unit",
            None,
            Some((900_000, 900_001)),
        ),
    )
    .unwrap();

    // 健全な索引の状態を作る: child_count を実カウントに揃える（本番も全ノードで一致している）。
    conn.execute_batch(
        "UPDATE memory_index_nodes
                 SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                     WHERE c.parent_id = memory_index_nodes.id);",
    )
    .unwrap();

    // カテゴリ所属: 消える topic と残る topic の両方に付ける。
    conn.execute_batch(
        "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
                 VALUES ('a1','topic-m','cat-1','2026-08-05T00:00:00+00:00'),
                        ('a1','topic-ok','cat-1','2026-08-05T00:00:00+00:00');",
    )
    .unwrap();

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let exists = |id: &str| -> i64 {
        count(&format!(
            "SELECT COUNT(*) FROM memory_index_nodes WHERE id = '{id}'"
        ))
    };
    let in_fts = |id: &str| -> i64 {
        count(&format!(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = '{id}'"
        ))
    };
    let fts_orphans = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_fts f
                 LEFT JOIN memory_index_nodes n ON n.id = f.node_id WHERE n.id IS NULL",
        )
    };
    let nodes_without_fts = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_nodes n
                 LEFT JOIN memory_index_fts f ON f.node_id = n.id WHERE f.node_id IS NULL",
        )
    };
    let child_count_of = |id: &str| -> i64 {
        count(&format!(
            "SELECT child_count FROM memory_index_nodes WHERE id = '{id}'"
        ))
    };
    let child_count_mismatch = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_nodes n
                 WHERE n.child_count <> (SELECT COUNT(*) FROM memory_index_nodes c
                                          WHERE c.parent_id = n.id)",
        )
    };
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 12);
    assert_eq!(fts_orphans(), 0);
    assert_eq!(nodes_without_fts(), 0);
    assert_eq!(child_count_mismatch(), 0, "仕込み時点では全ノード一致");
    assert_eq!(child_count_of("period-1"), 2);
    assert_eq!(child_count_of("period-2"), 1);
    assert_eq!(child_count_of("declroot-1"), 3);

    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 消えたもの（本体と FTS の両方から）。
    for id in ["sess-m", "sess-m2", "topic-m", "topic-m-nosrc", "unit-pure"] {
        assert_eq!(exists(id), 0, "{id} は消える");
        assert_eq!(in_fts(id), 0, "{id} は FTS からも同じ集合で消える");
    }
    // 残るもの。空になった period も残す（索引ビルダが同じ id で再利用するキー）。
    for id in [
        "period-1",
        "period-2",
        "declroot-1",
        "sess-ok",
        "topic-ok",
        "unit-mixed",
        "unit-empty",
    ] {
        assert_eq!(exists(id), 1, "{id} は残る");
        assert_eq!(in_fts(id), 1, "{id} の FTS も残る");
    }
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
    assert_eq!(fts_orphans(), 0, "FTS 孤児を作らない");
    assert_eq!(nodes_without_fts(), 0, "本体だけのノードも作らない");

    // 生き残る親の child_count が実カウントへ直っている。**子が 0 になった period-2 まで
    // 直ること**が肝（索引ビルダの再計算は子を持つ親しか書かないので、ここで直さないと
    // 永久にずれたまま残る）。
    assert_eq!(child_count_mismatch(), 0, "削除後も全ノードで一致");
    assert_eq!(child_count_of("period-1"), 1, "sess-m を失って 2→1");
    assert_eq!(child_count_of("period-2"), 0, "子が全て消えて 1→0");
    assert_eq!(child_count_of("declroot-1"), 2, "unit-pure を失って 3→2");

    // カテゴリ所属: 消えた topic への参照だけが消え、残る topic の行は残る。
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-m'"),
        0,
        "宙に浮く参照を残さない"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-ok'"),
        1,
        "残るノードの所属は残す"
    );

    // 生ログ側: メンテナンス 3 行が消え、通常 1 行は残る。
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
    assert_eq!(
        count(&format!(
            "SELECT COUNT(*) FROM memory_sessions WHERE id = {normal}"
        )),
        1
    );

    // FTS integrity-check（削除で索引が壊れていないこと）。
    conn.execute_batch("INSERT INTO memory_index_fts(memory_index_fts) VALUES('integrity-check');")
        .expect("memory_index_fts integrity");

    // 冪等: 版を 32 へ戻して再実行しても何も変わらない。生ログが消えた後は
    // 「範囲に 1 件以上ある」が成り立たないので、unit-mixed / unit-empty も対象外のまま。
    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
    assert_eq!(exists("unit-mixed"), 1, "再実行で本人の記憶を巻き込まない");
    assert_eq!(exists("unit-empty"), 1);
    assert_eq!(fts_orphans(), 0);
    assert_eq!(
        child_count_mismatch(),
        0,
        "再実行でも child_count は一致のまま"
    );
}
