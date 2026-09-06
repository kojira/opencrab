use super::*;

// 6. test_session_log_insert_and_fts
#[test]
fn test_session_log_insert_and_fts() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "The weather is sunny today.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "I enjoy programming in Rust.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };
    let log3 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Crabs live near the ocean.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(3),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();
    insert_session_log(&conn, &log3).unwrap();

    let results = search_session_logs(&conn, "agent-1", "sunny", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("sunny"));
}

/// #425: エコー行（表示専用の二重記録）は memory_sessions には入るが FTS には載せない。
/// 記憶検索（search_session_logs / count_matching_session_logs）に二重ヒット・過大計上を
/// 出さない。本体テーブルには残る（会話文脈の表示に使う）。
#[test]
fn heartbeat_channel_echo_excluded_from_fts_but_kept_in_table() {
    let conn = setup();

    // 通常の発話（印なし）と、同内容のエコー行（印つき）を入れる。
    let normal = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "discord-agent-1-111-222".to_string(),
        log_type: "speech".to_string(),
        content: "pineapple diagnostics report".to_string(),
        speaker_id: Some("human-9".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    let echo = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "discord-agent-1-111-222".to_string(),
        log_type: "speech".to_string(),
        content: "pineapple diagnostics report".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: None,
        metadata_json: Some(HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
        created_at: None,
    };
    insert_session_log(&conn, &normal).unwrap();
    insert_session_log(&conn, &echo).unwrap();

    // FTS 検索は印なしの 1 件だけ（エコーは載らない ＝ 二重ヒットしない）。
    let results = search_session_logs(&conn, "agent-1", "pineapple", 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "エコー行は FTS に載らないので検索ヒットは 1 件だけ: {results:?}"
    );
    assert_eq!(
        count_matching_session_logs(&conn, "agent-1", "pineapple").unwrap(),
        1,
        "count も過大計上しない"
    );

    // 本体テーブルには両方残る（会話文脈の表示に使う）。
    let rows = list_session_logs_by_session(&conn, "discord-agent-1-111-222").unwrap();
    assert_eq!(rows.len(), 2, "本体テーブルには印つき行も残る: {rows:?}");
}

/// #425: 判定は `source` フィールドの値で行い、キー順・空白の違いに強い。
/// 無関係な metadata（tool_call 等）・None は false。
#[test]
fn is_heartbeat_channel_echo_matches_on_source_field() {
    // 書き手が入れる正準形。
    assert!(is_heartbeat_channel_echo(Some(
        HEARTBEAT_CHANNEL_ECHO_METADATA
    )));
    // 空白・キー順が違っても source の値で判定する。
    assert!(is_heartbeat_channel_echo(Some(
        r#"{ "extra": 1, "source" : "heartbeat_channel_echo" }"#
    )));
    // 無関係な metadata は false（substring ゲートで早期に弾かれる）。
    assert!(!is_heartbeat_channel_echo(Some(
        r#"{"tool_calls_json":"[...]"}"#
    )));
    // source の値が別物なら false。
    assert!(!is_heartbeat_channel_echo(Some(r#"{"source":"other"}"#)));
    // None・空・壊れた JSON は false。
    assert!(!is_heartbeat_channel_echo(None));
    assert!(!is_heartbeat_channel_echo(Some("")));
    assert!(!is_heartbeat_channel_echo(Some("not json")));
}

/// #425 drift ガード: SQL 除外述語のリテラルが Rust 判定の source 定数と一致する。
/// 片方だけ値を変えると SQL と Rust が別の行を除外してしまう（規則が破れる）。
#[test]
fn exclude_echo_sql_matches_source_constant() {
    assert!(
        EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL.contains(HEARTBEAT_CHANNEL_ECHO_SOURCE),
        "SQL 述語のリテラルが source 定数と食い違っている: {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
    );
}

/// #425 不変テスト: エコー行（表示専用）は記憶系のどの経路（survey の地図/件数、
/// declare_window の total_remaining/窓境界、read_my_history の内容/range_total、
/// log_range_meta の件数、未索引件数、`nth_log_id_after` の窓境界）にも現れない。
///
/// 通常ログとエコーを**交互**に入れ、エコーが本来より手前に混ざる配置にする（末尾に足す
/// だけだと境界系が動かず恒真になる）。数えると変わる値を**絶対値**で固定するので、除外を
/// 外すと落ちる。
#[test]
fn heartbeat_channel_echo_invisible_to_all_memory_paths() {
    let conn = setup();
    let agent = "agent-1";
    let ins = |session: &str, speaker: &str, content: &str, echo: bool| {
        insert_session_log(
            &conn,
            &SessionLogRow {
                id: None,
                agent_id: agent.to_string(),
                session_id: session.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: echo.then(|| HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
                created_at: None,
            },
        )
        .unwrap();
    };
    // id1..id5 を交互に。id3 だけがエコー（本来の並びの中間に混ざる）。
    ins("discord-agent-1-111-a", "human", "normal 1", false); // id1
    ins("discord-agent-1-111-a", agent, "normal 2", false); // id2
    ins("discord-agent-1-111-a", agent, "ECHO utterance", true); // id3 (echo)
    ins("discord-agent-1-111-b", "human", "normal 4", false); // id4
    ins("discord-agent-1-111-b", agent, "normal 5", false); // id5

    // survey: 地図・件数・est_tokens にエコーは入らない（非エコー 4 件）。
    let survey = survey_my_history(&conn, agent, "day", 50).unwrap();
    assert_eq!(survey.total_logs, 4, "survey 総件数はエコーを数えない");
    let bucket_sum: i64 = survey.buckets.iter().map(|b| b.log_count).sum();
    assert_eq!(bucket_sum, 4, "バケット log_count 合計もエコーを数えない");

    // declare_window: total_remaining / log_count / 窓境界にエコーは入らない。
    let dw = declare_window(&conn, agent, 0, 100).unwrap();
    assert_eq!(
        dw.total_remaining, 4,
        "declare total_remaining はエコーを数えない"
    );
    assert_eq!(dw.log_count, 4, "declare 窓の log_count はエコーを数えない");
    assert_eq!(dw.from_id, Some(1));
    assert_eq!(dw.to_id, Some(5), "窓上端は最後の非エコー id");

    // nth_log_id_after: 「3 件目」はエコー(id3)を飛ばして id4（外すと id3 になる）。
    assert_eq!(
        nth_log_id_after(&conn, agent, 0, 3).unwrap(),
        Some(4),
        "窓境界（N 件目）はエコーを数えない"
    );

    // read_my_history: 内容にエコーは出ず、range_total も非エコー件数。
    let page = read_my_history(
        &conn,
        agent,
        &HistoryFilter::IdRange {
            from_id: 1,
            to_id: 1000,
        },
        None,
        100,
        100_000,
    )
    .unwrap();
    assert_eq!(page.range_total, 4, "read range_total はエコーを数えない");
    assert!(
        !page.rows.iter().any(|r| r.content.contains("ECHO")),
        "read の内容にエコーは現れない: {:?}",
        page.rows.iter().map(|r| &r.content).collect::<Vec<_>>()
    );

    // around 窓（resolve_around_window）でもエコーは内容に出ない。
    let around = read_my_history(
        &conn,
        agent,
        &HistoryFilter::Around {
            center_id: 4,
            radius: 5,
        },
        None,
        100,
        100_000,
    )
    .unwrap();
    assert!(
        !around.rows.iter().any(|r| r.content.contains("ECHO")),
        "around 読みでもエコーは現れない"
    );

    // log_range_meta: 宣言範囲の件数はエコーを数えない。
    let meta = log_range_meta(&conn, agent, 1, 1000).unwrap().unwrap();
    assert_eq!(meta.count, 4, "宣言範囲メタ件数はエコーを数えない");

    // 未索引件数: エコーは記憶材料でないので数に入らない。
    assert_eq!(
        get_unindexed_log_count(&conn, agent).unwrap(),
        4,
        "未索引件数はエコーを数えない"
    );

    // ただし本体テーブルには 5 行すべて残る（会話文脈の表示に使う）。
    let all = list_session_logs_by_session(&conn, "discord-agent-1-111-a").unwrap();
    assert!(
        all.iter().any(|r| r.content.contains("ECHO")),
        "本体テーブルにはエコー行が残る（表示専用）"
    );
}

// 7. test_fts_multi_word_search
#[test]
fn test_fts_multi_word_search() {
    let conn = setup();

    let log1 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Quantum computing will revolutionize cryptography.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    let log2 = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Classical computing is still dominant.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(2),
        metadata_json: None,
        created_at: None,
    };

    insert_session_log(&conn, &log1).unwrap();
    insert_session_log(&conn, &log2).unwrap();

    let results = search_session_logs(&conn, "agent-1", "quantum cryptography", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("Quantum"));
}

// 8. test_fts_no_results
#[test]
fn test_fts_no_results() {
    let conn = setup();

    let log = SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Hello world from the test.".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
        created_at: None,
    };
    insert_session_log(&conn, &log).unwrap();

    let results = search_session_logs(&conn, "agent-1", "nonexistenttermxyz", 10).unwrap();
    assert!(results.is_empty());
}

/// `list_recent_session_logs_of_type`: **SQL 側**で log_type を絞る（#404 / #405）。
///
/// 呼び出し側で絞ると「生の直近 N 件を取ってから捨てる」ことになり、ツール往復の多い
/// セッションでは目的の種別が N の一部しか残らない。ここで固定するのは
/// 「limit は絞ったあとの件数」「他種別が混ざらない」「id DESC で返る」。
#[test]
fn list_recent_session_logs_of_type_filters_in_sql() {
    let conn = setup();
    let mk = |log_type: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: "a1".to_string(),
        session_id: "s1".to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: Some("a1".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    // 発言 1 件のあとにツール往復 20 件、さらに発言 3 件。
    insert_session_log(&conn, &mk("speech", "oldest speech")).unwrap();
    for i in 0..20 {
        insert_session_log(&conn, &mk("tool_result", &format!("tool {i}"))).unwrap();
    }
    for i in 0..3 {
        insert_session_log(&conn, &mk("speech", &format!("speech {i}"))).unwrap();
    }
    // 別セッションは混ざらない。
    let mut other = mk("speech", "other session");
    other.session_id = "s2".to_string();
    insert_session_log(&conn, &other).unwrap();

    // 生の直近 10 件を取ってから絞ると、ツール往復に押し出されて古い発言が落ちる。
    let raw = list_recent_session_logs(&conn, "s1", 10).unwrap();
    assert!(
        !raw.iter().any(|l| l.content == "oldest speech"),
        "前提: 生の窓では古い発言が窓の外にある"
    );

    let speech = list_recent_session_logs_of_type(&conn, "s1", "speech", 10).unwrap();
    assert_eq!(speech.len(), 4, "limit は絞ったあとの件数");
    assert!(speech.iter().all(|l| l.log_type == "speech"));
    assert!(speech.iter().all(|l| l.session_id == "s1"));
    // id DESC（呼び出し側が reverse する前提）。
    assert_eq!(speech[0].content, "speech 2");
    assert_eq!(speech[3].content, "oldest speech");

    // limit は効く。
    let limited = list_recent_session_logs_of_type(&conn, "s1", "speech", 2).unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].content, "speech 2");

    // 該当が無ければ空。
    assert!(
        list_recent_session_logs_of_type(&conn, "s1", "inner_voice", 10)
            .unwrap()
            .is_empty()
    );
}
