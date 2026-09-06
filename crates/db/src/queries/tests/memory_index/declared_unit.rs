use super::*;

// ============================================
// 記憶の単位（宣言 / issue #379 #376 段階1）
// ============================================

/// 生ログを 1 件挿入して id を返す（created_at は now）。
fn ins_log(conn: &Connection, agent: &str, session: &str, content: &str) -> i64 {
    insert_session_log(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "message".to_string(),
            content: content.to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap()
}

fn fts_has_node(conn: &Connection, node_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
        params![node_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// **設計の核**: 宣言ユニット（node_type='unit'）は time-series・タグ整理の worklist 系
/// クエリに 1 件も出ない（構造的分離）。一方 browse（get_index_tree）と search（FTS）には出る。
#[test]
fn declared_unit_excluded_from_all_worklists_but_visible_to_browse_and_search() {
    let conn = setup();
    // session_log topic を 2 件（keywords 未設定＝ backfill 対象）。
    seed_topic(
        &conn,
        "a1",
        "t1",
        "s1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "t2",
        "s2",
        "2026-08-02T00:00:00Z",
        Some(20),
        "session_log",
    );
    // 生ログを積んで宣言範囲を作る。
    for i in 0..5 {
        ins_log(&conn, "a1", "sess", &format!("発話 {i}"));
    }
    let now = "2026-08-04T00:00:00Z";
    let unit = record_memory_unit(
        &conn,
        "a1",
        "Rust の所有権を理解した週",
        "所有権と借用の話",
        1,
        5,
        Some("2026-08-04"),
        Some("2026-08-04"),
        now,
    )
    .unwrap();
    assert_eq!(unit.node_type, "unit");
    assert_eq!(unit.source_type, "declared");
    assert_eq!(unit.short_id.as_deref(), Some("u1"));

    // --- worklist 系: 宣言ユニットは 1 件も出ない（session_log topic だけ） ---
    // (1) スリープ整理（新規側）
    assert_eq!(count_organize_topics(&conn, "a1", None, 100).unwrap(), 2);
    let wl = list_organize_topics(&conn, "a1", None, 100, 50).unwrap();
    assert_eq!(wl.len(), 2);
    assert!(wl.iter().all(|n| n.node_type == "topic"));
    // (2) スリープ整理（遡り側）
    let before = ("2027-01-01T00:00:00Z", "");
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        2
    );
    let bl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert!(bl.iter().all(|n| n.id != unit.id));
    // (3) タグ割当 worklist（未分類 topic）
    let unassigned = list_unassigned_topics(&conn, "a1", 50).unwrap();
    assert_eq!(unassigned.len(), 2, "unit は未分類 topic に混ざらない");
    assert!(unassigned.iter().all(|n| n.id != unit.id));
    // (4) keywords バックフィル worklist（unit も keywords='[]' だが対象外）
    let missing = list_topics_missing_keywords(&conn, "a1", 50).unwrap();
    assert_eq!(missing.len(), 2);
    assert!(missing.iter().all(|n| n.id != unit.id));

    // --- browse / search: 宣言ユニットは見える（意図どおり） ---
    let tree = get_index_tree(&conn, "a1").unwrap();
    assert!(
        tree.iter()
            .any(|n| n.id == unit.id && n.node_type == "unit"),
        "browse（get_index_tree）には宣言ユニットが出る"
    );
    let hits = search_index_nodes(&conn, "a1", "所有権", 10, None).unwrap();
    assert!(
        hits.iter().any(|h| h.node_id == unit.id),
        "search（FTS）で宣言ユニットが引ける"
    );
}

/// 月次ロールアップの topic 数集計（count_topics_per_period）は宣言ユニットで水増しされない。
#[test]
fn declared_unit_does_not_inflate_rollup_topic_count() {
    let conn = setup();
    // root→period→session→topic の time-series ツリー。
    for (id, ntype, parent) in [
        ("root-a1", "root", None),
        ("p1", "period", Some("root-a1")),
        ("s1", "session", Some("p1")),
    ] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: parent.map(String::from),
                node_type: ntype.to_string(),
                source_type: "session_log".to_string(),
                title: id.to_string(),
                summary: String::new(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-06-01T00:00:00Z".to_string(),
                updated_at: "2026-06-01T00:00:00Z".to_string(),
                short_id: Some(id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    insert_index_node(&conn, &mk_topic_node("tp1", "a1", "topic1", "s", &[])).unwrap();
    // tp1 を session s1 の下へ。
    conn.execute(
        "UPDATE memory_index_nodes SET parent_id = 's1' WHERE id = 'tp1'",
        [],
    )
    .unwrap();

    let before = count_topics_per_period(&conn, "a1").unwrap();
    // 宣言ユニットを足す。
    ins_log(&conn, "a1", "sess", "x");
    record_memory_unit(
        &conn,
        "a1",
        "宣言",
        "",
        1,
        1,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    let after = count_topics_per_period(&conn, "a1").unwrap();
    assert_eq!(before, after, "宣言ユニットは period の topic 数を変えない");
    assert_eq!(after.get("p1").copied(), Some(1));
}

/// record → retract で原状復帰（宣言ノード・FTS・member が消え、生ログは不変）。
#[test]
fn record_then_retract_restores_state_and_leaves_raw_logs() {
    let conn = setup();
    for i in 0..4 {
        ins_log(&conn, "a1", "sess", &format!("log {i}"));
    }
    let logs_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| r.get(0))
        .unwrap();

    let unit = record_memory_unit(
        &conn,
        "a1",
        "宣言タイトル",
        "要約",
        1,
        4,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    // タグを 2 つ付ける（member 行が付く）。
    tag_topic(
        &conn,
        "a1",
        &unit.id,
        &["タグA".into(), "タグB".into()],
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    assert!(fts_has_node(&conn, &unit.id), "record 後 FTS 行あり");
    assert_eq!(members_for(&conn, "a1", &unit.id), 2, "タグ 2 件の member");

    // retract。
    let removed = retract_memory_unit(&conn, "a1", "u1").unwrap();
    assert_eq!(removed, unit.id);
    assert!(
        get_index_node(&conn, &unit.id).unwrap().is_none(),
        "宣言ノードが消える"
    );
    assert!(!fts_has_node(&conn, &unit.id), "FTS 孤児を残さない");
    assert_eq!(members_for(&conn, "a1", &unit.id), 0, "member 行が消える");
    // 生ログは不変。
    let logs_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logs_before, logs_after, "生ログは消えない・変わらない");
}

/// retract は宣言ユニット以外（session_log topic 等）を消せない（安全ガード）。
#[test]
fn retract_refuses_non_unit_nodes() {
    let conn = setup();
    seed_topic(
        &conn,
        "a1",
        "t1",
        "s1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    let err = retract_memory_unit(&conn, "a1", "s1").unwrap_err();
    assert!(
        err.to_string().contains("宣言ユニット"),
        "session_log topic は retract できない: {err}"
    );
    assert!(
        get_index_node(&conn, "t1").unwrap().is_some(),
        "対象の topic は消えていない"
    );
}

/// エージェント間で宣言を混ぜない（a1 の unit を a2 名義で retract できない）。
#[test]
fn retract_is_agent_scoped() {
    let conn = setup();
    ins_log(&conn, "a1", "sess", "x");
    let unit = record_memory_unit(
        &conn,
        "a1",
        "a1 の宣言",
        "",
        1,
        1,
        None,
        None,
        "2026-08-04T00:00:00Z",
    )
    .unwrap();
    // a2 名義では見つからない。
    assert!(retract_memory_unit(&conn, "a2", &unit.id).is_err());
    assert!(get_index_node(&conn, &unit.id).unwrap().is_some());
}

/// read_my_history は行数キャップを守り、カーソルで続きが読める。
#[test]
fn read_my_history_respects_row_cap_and_cursor() {
    let conn = setup();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(ins_log(&conn, "a1", "sess", &format!("発話{i}")));
    }
    let filter = HistoryFilter::Session("sess".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 2, 100_000).unwrap();
    assert_eq!(page.returned, 2);
    assert_eq!(page.range_total, 5, "範囲全体の件数は常に返す");
    assert!(page.truncated);
    assert_eq!(page.next_from_id, Some(ids[2]));
    // 続きをカーソルで読む。
    let page2 = read_my_history(&conn, "a1", &filter, page.next_from_id, 2, 100_000).unwrap();
    assert_eq!(page2.returned, 2);
    assert_eq!(page2.rows[0].id, Some(ids[2]));
    let page3 = read_my_history(&conn, "a1", &filter, page2.next_from_id, 2, 100_000).unwrap();
    assert_eq!(page3.returned, 1);
    assert!(!page3.truncated);
    assert_eq!(page3.next_from_id, None);
}

/// read_my_history は総文字数キャップを守る（ただし先頭 1 行は必ず返す＝前進保証）。
#[test]
fn read_my_history_respects_char_cap_with_progress_guarantee() {
    let conn = setup();
    // 各 30 文字の本文を 4 件。char_cap=50 なら 1 行で超えるので先頭のみ返る。
    for i in 0..4 {
        ins_log(&conn, "a1", "sess", &"あ".repeat(30));
        let _ = i;
    }
    let filter = HistoryFilter::Session("sess".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 200, 50).unwrap();
    assert_eq!(
        page.returned, 1,
        "先頭 1 行は char_cap を超えても返す（前進保証）"
    );
    assert!(page.truncated);
    assert!(page.next_from_id.is_some());
}

/// read_my_history は agent_id でスコープされる（他エージェントのログを返さない）。
#[test]
fn read_my_history_is_agent_scoped() {
    let conn = setup();
    ins_log(&conn, "a1", "shared", "a1 の発話");
    ins_log(&conn, "a2", "shared", "a2 の発話");
    let filter = HistoryFilter::Session("shared".to_string());
    let page = read_my_history(&conn, "a1", &filter, None, 200, 100_000).unwrap();
    assert_eq!(page.returned, 1);
    assert_eq!(page.rows[0].content, "a1 の発話");
}

/// survey_my_history の集計（件数・セッション数・id 範囲・種別内訳・バケット上限）。
#[test]
fn survey_my_history_aggregates_and_caps_buckets() {
    let conn = setup();
    // created_at と log_type を制御するため直接 INSERT する。
    let rows = [
        ("2026-08-01T09:00:00Z", "message", "sA"),
        ("2026-08-01T10:00:00Z", "message", "sA"),
        ("2026-08-01T11:00:00Z", "reaction", "sB"),
        ("2026-08-02T09:00:00Z", "message", "sC"),
    ];
    for (ts, lt, sess) in rows {
        conn.execute(
            "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
             VALUES ('a1', ?1, ?2, 'x', ?3)",
            params![sess, lt, ts],
        )
        .unwrap();
    }
    let survey = survey_my_history(&conn, "a1", "day", 60).unwrap();
    assert_eq!(survey.total_logs, 4);
    assert_eq!(survey.total_sessions, 3);
    assert_eq!(survey.total_buckets, 2);
    assert_eq!(survey.buckets.len(), 2);
    assert!(!survey.truncated);
    // 新しいバケットが先頭（2026-08-02）。
    assert_eq!(survey.buckets[0].bucket, "2026-08-02");
    let day1 = survey
        .buckets
        .iter()
        .find(|b| b.bucket == "2026-08-01")
        .unwrap();
    assert_eq!(day1.log_count, 3);
    assert_eq!(day1.session_count, 2);
    assert_eq!(day1.type_counts.get("message").copied(), Some(2));
    assert_eq!(day1.type_counts.get("reaction").copied(), Some(1));
    // サイズ地図（#386）: content='x' が 3 件で 3 文字、est は 3*2/3=2。
    assert_eq!(day1.content_chars, 3);
    assert_eq!(day1.est_tokens, 2);
    // 全体の総文字数・概算トークンも返す（バケットを落としても残る）。
    assert_eq!(survey.total_content_chars, 4);
    assert_eq!(survey.total_est_tokens, 2);
    // バケット上限で古い側を落とす。
    let capped = survey_my_history(&conn, "a1", "day", 1).unwrap();
    assert_eq!(capped.buckets.len(), 1);
    assert!(capped.truncated);
    assert_eq!(capped.total_buckets, 2, "全体の総バケット数は落とさず返す");
}

// ---- 宣言ランの窓を本人が決める（#394）----

/// `nth_log_id_after` は **id の差ではなく生ログの件数**で数える（id は全エージェント共通の
/// 採番なので、1 エージェントぶんの id は疎らに飛ぶ）。件数が足りなければ最後の id へ丸める。
#[test]
fn nth_log_id_after_counts_rows_not_id_gaps() {
    let conn = setup();
    // a1 と a2 を交互に入れ、a1 の id を飛び飛びにする。
    for i in 0..10 {
        for agent in ["a1", "a2"] {
            conn.execute(
                "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
                 VALUES (?1, 's1', 'message', ?2, '2026-08-01T00:00:00Z')",
                params![agent, format!("{agent}-{i}")],
            )
            .unwrap();
        }
    }
    let a1_ids: Vec<i64> = conn
        .prepare("SELECT id FROM memory_sessions WHERE agent_id='a1' ORDER BY id ASC")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(a1_ids.len(), 10);
    assert!(a1_ids[1] - a1_ids[0] > 1, "id が飛んでいる前提が崩れている");

    // cursor=0 から 1 件目 / 3 件目 / 10 件目。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 1).unwrap(),
        Some(a1_ids[0])
    );
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 3).unwrap(),
        Some(a1_ids[2])
    );
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 10).unwrap(),
        Some(a1_ids[9])
    );
    // 足りなければ最後（あるだけ進める）。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 999).unwrap(),
        Some(a1_ids[9])
    );
    // cursor より後ろだけを数える。
    assert_eq!(
        nth_log_id_after(&conn, "a1", a1_ids[4], 2).unwrap(),
        Some(a1_ids[6])
    );
    // 1 件も無ければ None（＝進める先が無い）。
    assert_eq!(nth_log_id_after(&conn, "a1", a1_ids[9], 1).unwrap(), None);
    // 0 / 負の n は 1 件目に丸める（呼び出し側の 0 除算・逆走を防ぐ）。
    assert_eq!(
        nth_log_id_after(&conn, "a1", 0, 0).unwrap(),
        Some(a1_ids[0])
    );
}

/// 窓の希望（#394）は JSON で 1 列に往復し、壊れた JSON は「希望なし」に倒れる。
#[test]
fn declare_window_pref_roundtrips_and_tolerates_garbage() {
    let conn = setup();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);

    let pref = DeclareWindowPref {
        next_from_id: Some(23_600),
        window_size: Some(450),
        note: Some("材料が薄かったので広げる".to_string()),
        updated_at: Some("2026-08-05T00:00:00Z".to_string()),
        partial_streak: Some(2),
    };
    set_memory_declare_window(&conn, "a1", Some(&pref)).unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), Some(pref));

    // 位置と理由を消して広さは残す（ランが位置を使い切ったときの形）。
    let sticky = DeclareWindowPref {
        next_from_id: None,
        window_size: Some(450),
        ..Default::default()
    };
    set_memory_declare_window(&conn, "a1", Some(&sticky)).unwrap();
    let got = get_memory_declare_window(&conn, "a1").unwrap().unwrap();
    assert_eq!(got.next_from_id, None);
    assert_eq!(got.note, None);
    assert_eq!(got.partial_streak, None);
    assert_eq!(got.window_size, Some(450));

    // 旧い形（`partial_streak` が無い JSON）も読める（列は後から足したフィールド）。
    conn.execute(
        "UPDATE agent_memory_index_config SET memory_declare_window = '{\"window_size\":450}' WHERE agent_id='a1'",
        [],
    )
    .unwrap();
    let old = get_memory_declare_window(&conn, "a1").unwrap().unwrap();
    assert_eq!(old.window_size, Some(450));
    assert_eq!(old.partial_streak, None);

    // 隣の列（宣言ランのマーカー）は触らない。
    set_memory_declare_cursor(&conn, "a1", "2026-08-05T00:00:00Z|23594").unwrap();
    set_memory_declare_window(&conn, "a1", Some(&sticky)).unwrap();
    assert_eq!(
        get_memory_declare_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-05T00:00:00Z|23594")
    );

    // NULL へ戻せる。
    set_memory_declare_window(&conn, "a1", None).unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);

    // 壊れた JSON はエラーにせず「希望なし」（＝従来どおりの窓）に倒れる。
    conn.execute(
        "UPDATE agent_memory_index_config SET memory_declare_window = 'not json' WHERE agent_id='a1'",
        [],
    )
    .unwrap();
    assert_eq!(get_memory_declare_window(&conn, "a1").unwrap(), None);
}
