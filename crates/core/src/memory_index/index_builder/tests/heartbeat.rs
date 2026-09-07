    // ================================================================
    // #374: 何もしなかったハートビート（idle）を topic 化しない
    // ================================================================

    /// heartbeat セッションに任意の log_type / speaker の行を投入するヘルパー。
    fn insert_hb_row(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        log_type: &str,
        speaker: &str,
        content: &str,
    ) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(conn, &log).unwrap();
    }

    /// heartbeat セッションに agent の speech 行を投入するヘルパー。
    fn insert_heartbeat_speech(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        content: &str,
    ) {
        insert_hb_row(conn, agent_id, session_id, "speech", agent_id, content);
    }

    /// 毎 tick 注入される heartbeat のプロンプト scaffolding 行を投入するヘルパー。
    fn insert_heartbeat_prompt(conn: &rusqlite::Connection, agent_id: &str, session_id: &str) {
        insert_hb_row(
            conn,
            agent_id,
            session_id,
            "system",
            "heartbeat",
            "[ハートビート] 現在の会話「x」。出力形式: SPEAK/LEARN/IDLE のいずれか。",
        );
    }

    /// idle 判定が過去データ（旧 SPEAK/LEARN/IDLE 語彙）の分類と一致することを直接確認する。
    #[test]
    fn test_is_idle_heartbeat_speech_classification() {
        // speaker はデフォルトで自分（"a"）。
        let mk = |content: &str, log_type: &str| opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "a".to_string(),
            session_id: "heartbeat-a-c".to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: Some("a".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        // SPEAK: も LEARN も無い → idle
        assert!(is_idle_heartbeat_speech(&mk("IDLE", "speech"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("  IDLE  ", "speech"), "a"));
        // SPEAK: の後に非空 → 実あり
        assert!(!is_idle_heartbeat_speech(
            &mk("SPEAK: hello", "speech"),
            "a"
        ));
        assert!(!is_idle_heartbeat_speech(
            &mk("考えた結果\nSPEAK: みんなおはよう", "speech"),
            "a"
        ));
        // SPEAK: が空 → idle（main.rs と同基準）
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:", "speech"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:   ", "speech"), "a"));
        // LEARN を含む → 実あり
        assert!(!is_idle_heartbeat_speech(&mk("LEARN", "speech"), "a"));
        assert!(!is_idle_heartbeat_speech(
            &mk("learn something new", "speech"),
            "a"
        ));
        // speech 以外の log_type は常に実あり扱い（heartbeat の応答は speech のみ）
        assert!(!is_idle_heartbeat_speech(&mk("IDLE", "system"), "a"));
        // 話者ガード: 他者の発言は本文が idle 風でも idle 扱いにしない（材料に残す）
        assert!(!is_idle_heartbeat_speech(
            &mk("IDLE", "speech"),
            "other-agent"
        ));
    }

    /// #517: #515 以降、IDLE の記録は「`IDLE: <なぜ見送ったか>`」と本人の言葉の理由を持つ。
    /// 理由つきは材料として意味があるので**索引に残す**。無内容の裸マーカーだけを除外する。
    /// サンプルは本番 DB（`?mode=ro`）の実データから採取。
    #[test]
    fn idle_speech_keeps_reasoned_records_517() {
        let mk = |content: &str| opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "a".to_string(),
            session_id: "heartbeat-a-c".to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some("a".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        // 無内容の裸マーカー・空文字は従来どおり idle（除外）— 回帰ガード。
        assert!(is_idle_heartbeat_speech(&mk("IDLE"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("  IDLE  "), "a"));
        assert!(is_idle_heartbeat_speech(&mk("IDLE:"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("IDLE:   "), "a"));
        assert!(
            is_idle_heartbeat_speech(&mk("NO_REPLY"), "a"),
            "裸の決定マーカーは除外"
        );
        assert!(is_idle_heartbeat_speech(&mk(""), "a"), "空文字は除外");

        // #515 の理由つき IDLE（本番実サンプル）は実ありとして残す。
        for s in [
            "IDLE: 自分自身を直す開発ツールの再帰ネタで、TLへ自然に混ざった。",
            "IDLE: まだ30分経っていない。静かに待つ。",
            "IDLE: TLの新着を絞り込み中。",
            // 改行後に本文が続く形（マーカー行の後に散文）も残す。
            "IDLE\n\n特に話題もないしのんびりしてるよ〜☀️",
        ] {
            assert!(
                !is_idle_heartbeat_speech(&mk(s), "a"),
                "理由つき IDLE を落としている: {s}"
            );
        }

        // マーカー無しの散文（本番実サンプル）も中身があるので残す。
        assert!(!is_idle_heartbeat_speech(&mk("確認中だよ〜。"), "a"));

        // SPEAK/LEARN の扱いは不変。
        assert!(!is_idle_heartbeat_speech(&mk("SPEAK: おはよう"), "a"));
        assert!(is_idle_heartbeat_speech(&mk("SPEAK:"), "a"));
        assert!(!is_idle_heartbeat_speech(&mk("LEARN"), "a"));
    }

    /// #517: 理由判定ヘルパの単体。先頭全大文字マーカー＋任意の `:` を剥いだ残りの有無で決める。
    #[test]
    fn idle_decision_has_no_reason_unit_517() {
        // 裸マーカー / 空 → 理由なし（true）。
        assert!(idle_decision_has_no_reason("IDLE"));
        assert!(idle_decision_has_no_reason("IDLE:"));
        assert!(idle_decision_has_no_reason("NO_REPLY"));
        assert!(idle_decision_has_no_reason("  IDLE  "));
        assert!(idle_decision_has_no_reason(""));
        // 理由つき / 散文 → 理由あり（false）。
        assert!(!idle_decision_has_no_reason("IDLE: 理由"));
        assert!(!idle_decision_has_no_reason("IDLE:理由"));
        assert!(!idle_decision_has_no_reason("IDLE\n\n本文"));
        assert!(!idle_decision_has_no_reason("確認中だよ〜。"));
    }

    /// is_heartbeat_noise: プロンプト scaffolding と idle speech を除き、
    /// tool/inner_voice/実ある speech は残す。
    #[test]
    fn test_is_heartbeat_noise_classification() {
        let mk =
            |content: &str, log_type: &str, speaker: &str| opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a".to_string(),
                session_id: "heartbeat-a-c".to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            };
        // 毎tick のプロンプト scaffolding（system + speaker=heartbeat）→ ノイズ
        assert!(is_heartbeat_noise(
            &mk("[ハートビート] ...", "system", "heartbeat"),
            "a"
        ));
        // idle speech（自分）→ ノイズ
        assert!(is_heartbeat_noise(&mk("IDLE", "speech", "a"), "a"));
        // 実のある speech → 残す
        assert!(!is_heartbeat_noise(&mk("SPEAK: hi", "speech", "a"), "a"));
        assert!(!is_heartbeat_noise(&mk("LEARN x", "speech", "a"), "a"));
        // tool / inner_voice は実活動 → 残す
        assert!(!is_heartbeat_noise(&mk("call foo", "tool_call", "a"), "a"));
        assert!(!is_heartbeat_noise(&mk("result", "tool_result", "a"), "a"));
        assert!(!is_heartbeat_noise(
            &mk("考えている", "inner_voice", "a"),
            "a"
        ));
        // system でも speaker が heartbeat 以外なら残す
        assert!(!is_heartbeat_noise(&mk("なにか", "system", "a"), "a"));
        // 他者の発言は本文が idle 風でも残す（相手の言葉を落とさない）
        assert!(!is_heartbeat_noise(&mk("IDLE", "speech", "other"), "a"));
    }

    /// 純idle だけの heartbeat グループ（毎tick のプロンプト + idle speech のみ）
    /// → topic を作らず、watermark は前進する。
    #[tokio::test]
    async fn test_heartbeat_idle_only_no_topic_but_watermark_advances() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // 3 tick 分。各 tick は「プロンプト行 + idle の応答」の 2 行。
        for _ in 0..3 {
            insert_heartbeat_prompt(&db_conn, "agent-1", sid);
            insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        }
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        // 6 行は処理対象として消費される（logs_indexed は取得件数）
        assert_eq!(result.logs_indexed, 6);

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            // 実質行が無いので topic も session も作られない（root のみ）
            assert!(
                tree.iter().all(|n| n.node_type != "topic"),
                "純idle なら topic は作られない"
            );
            assert!(
                tree.iter().all(|n| n.node_type != "session"),
                "純idle なら session ノードも作られない"
            );
            // watermark は最終ログ ID まで前進している
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 6,
                "topic を作らなくても watermark は前進する"
            );
        }

        // 再ビルドで同じログを取り直さない（無限ループにならない）
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進により再取得しない");
    }

    /// decision が idle でも tool/inner_voice の実活動があれば topic は作られる。
    #[tokio::test]
    async fn test_heartbeat_idle_decision_with_tool_activity_creates_topic() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        insert_heartbeat_prompt(&db_conn, "agent-1", sid);
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "inner_voice",
            "agent-1",
            "状況を確認しよう",
        );
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "tool_call",
            "agent-1",
            "search(x)",
        );
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "tool_result",
            "agent-1",
            "found y",
        );
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "tool/inner_voice の実活動があれば topic は作られる"
        );
    }

    /// 話者ガード: heartbeat セッションに他者の発言が混ざっていたら、自分は idle でも
    /// topic を作り、他者の言葉を要約材料に残す（相手の言葉を落とさない）。
    #[tokio::test]
    async fn test_heartbeat_others_speech_is_kept() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // 自分は静観、しかし相手が話しかけてきた（本文は SPEAK:/LEARN を含まない実質発言）
        insert_heartbeat_prompt(&db_conn, "agent-1", sid);
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "speech",
            "someone-else",
            "ねえ、これどう思う？相手からの実質発言marker",
        );
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(
                tree.iter().any(|n| n.node_type == "topic"),
                "他者の発言があれば topic は作られる"
            );
        }
        // 他者の発言は要約材料に含まれ、自分の idle 行は除かれる
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("相手からの実質発言marker"),
            "他者の発言は材料に残る"
        );
    }

    /// idle と実のある tick が混在する heartbeat グループ → topic は作られ、
    /// 要約の材料から idle 行が除かれている。被覆範囲は全ログを跨ぐ。
    #[tokio::test]
    async fn test_heartbeat_mixed_creates_topic_excluding_idle_material() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        // #517: idle 行は実在形の裸マーカー（`IDLE`）を使う。以前は "IDLE静観marker" と
        // マーカーに内容を直結した合成文字列だったが、#517 で「中身があるか」判定に変えた
        // 結果その形は理由つき扱いで残る（意図どおり）。無内容の裸 idle が材料から除かれる
        // ことをここで担保する（理由つき idle を残すことは unit テストが担保）。
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "SPEAK: 実のある発言unique");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let topic = tree
                .iter()
                .find(|n| n.node_type == "topic")
                .expect("実のある行があれば topic は作られる");
            // 被覆範囲は idle を含む全ログを跨ぐ（watermark/カバレッジ維持）
            assert_eq!(topic.start_log_id, Some(1));
            assert_eq!(topic.end_log_id, Some(3));
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 3);
        }

        // LLM に渡した要約材料から idle 行が除かれ、実のある行だけが含まれる
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("実のある発言unique"),
            "実のある行は材料に含まれる"
        );
        assert!(
            !prompt.contains("IDLE"),
            "無内容の裸 idle 行は要約材料から除かれる"
        );
    }

    /// heartbeat の LEARN 行は実ありとして残り、topic が作られる。
    #[tokio::test]
    async fn test_heartbeat_learn_row_is_substantive() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "heartbeat-agent-1-chan-1";
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "IDLE");
        insert_heartbeat_speech(&db_conn, "agent-1", sid, "LEARN 新しい知見を得た");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "LEARN 行があれば topic は作られる"
        );
    }

    /// 通常（非 heartbeat）セッションは idle フィルタの対象外。本文が "IDLE" でも
    /// 従来どおり topic が作られる。
    #[tokio::test]
    async fn test_non_heartbeat_session_bare_idle_marker_is_filtered_573() {
        // #573 Stage A: idle ノイズ除外は接頭辞ゲートを外し全セッションへ適用する。
        // 通常セッション（`heartbeat-` で始まらない）でも、中身の無い裸マーカーだけの
        // バッチは topic を作らない（実会話セッションの裸 `NO_REPLY` が材料を汚さない）。
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_heartbeat_speech(&db_conn, "agent-1", "discord-agent-1-g-c", "NO_REPLY");
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            !tree.iter().any(|n| n.node_type == "topic"),
            "通常セッションでも裸マーカーのみのバッチは topic を作らない"
        );
    }

    /// #573 Stage A: 接頭辞ゲートを外しても**中身のある発話は落とさない**（過剰フィルタ
    /// でないことの包含確認）。通常セッションに実のある speech があれば topic が作られる。
    #[tokio::test]
    async fn test_non_heartbeat_session_substantive_speech_still_indexed_573() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_heartbeat_speech(
            &db_conn,
            "agent-1",
            "discord-agent-1-g-c",
            "IDLE: 相手が寝る前の挨拶をしていたので静かに見送った。",
        );
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert!(
            tree.iter().any(|n| n.node_type == "topic"),
            "中身のある IDLE 理由（#517）は通常セッションでも材料に残り topic 化する"
        );
    }

    /// #425 表示専用エコー行を投入するヘルパー。実会話（discord）セッションに、
    /// 印つき speech として入れる。
    fn insert_echo(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, content: &str) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(conn, &log).unwrap();
    }

    /// #425: エコー行だけのバッチ（実会話セッションに印つき行しか無い）→ topic を作らず、
    /// watermark は前進する。エコー行が「永遠に未索引」で残ってバッチを詰まらせない
    /// （#416 と同族の「無言で進まない」を作らない）。再ビルドで同じ行を取り直さない。
    #[tokio::test]
    async fn test_heartbeat_echo_only_no_topic_but_watermark_advances() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "discord-agent-1-111-222";
        insert_echo(&db_conn, "agent-1", sid, "エコー発話1");
        insert_echo(&db_conn, "agent-1", sid, "エコー発話2");
        insert_echo(&db_conn, "agent-1", sid, "エコー発話3");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        // 3 行は取得（消費）される。
        assert_eq!(result.logs_indexed, 3);

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(
                tree.iter().all(|n| n.node_type != "topic"),
                "エコーだけなら topic は作られない（記憶材料に入れない）"
            );
            assert!(
                tree.iter().all(|n| n.node_type != "session"),
                "エコーだけなら session ノードも作られない"
            );
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 3,
                "topic を作らなくても watermark は前進する（バッチが詰まらない）"
            );
        }

        // 再ビルドで同じログを取り直さない（無限ループにならない）。
        let r2 = IndexBuilder::build_incremental(&conn, "agent-1", &MockLlm, "m", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進により再取得しない");
    }

    /// #425: 実会話セッションに実発言とエコーが混在 → topic は作られるが、要約材料には
    /// エコー行が入らない（記憶索引・宣言材料はこの PR の前後で不変）。被覆範囲は
    /// エコーを含む全ログを跨ぐ（watermark 維持）。
    #[tokio::test]
    async fn test_heartbeat_echo_excluded_from_index_material() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let sid = "discord-agent-1-111-222";
        // 他者の実発言（材料に残る）→ 本人の HB エコー（材料から除く）。
        insert_hb_row(
            &db_conn,
            "agent-1",
            sid,
            "speech",
            "someone-else",
            "他者の実発言substantivemarker",
        );
        insert_echo(&db_conn, "agent-1", sid, "本人のHBエコーechomarker");
        let conn = opencrab_db::Db::from_connection(db_conn);

        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };
        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "m", 50, "", None)
            .await
            .unwrap();

        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let topic = tree
                .iter()
                .find(|n| n.node_type == "topic")
                .expect("他者の実発言があれば topic は作られる");
            // 被覆範囲はエコーを含む全ログを跨ぐ（カバレッジ・watermark 維持）。
            assert_eq!(topic.start_log_id, Some(1));
            assert_eq!(topic.end_log_id, Some(2));
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 2);
        }

        // LLM に渡した要約材料からエコー行が除かれ、他者の実発言だけが含まれる。
        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("").to_string();
        assert!(
            prompt.contains("substantivemarker"),
            "他者の実発言は材料に残る"
        );
        assert!(
            !prompt.contains("echomarker"),
            "エコー行は要約材料から除かれる（記憶索引に入れない）"
        );
    }
