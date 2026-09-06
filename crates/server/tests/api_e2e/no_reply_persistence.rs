// ============ (#899) NO_REPLY のみの応答は speech として保存しない（REST 境界） ============

/// `POST /api/agents/{id}/messages` を叩き、(status, responses 配列, session_id) を返す。
async fn agent_message(
    app: Router,
    agent_id: &str,
    user_id: &str,
    content: &str,
) -> (Router, Vec<serde_json::Value>, String) {
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({ "content": content, "user_id": user_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "messages 200 でない: {resp}");
    let responses = resp["responses"].as_array().cloned().unwrap_or_default();
    let session_id = resp["session_id"].as_str().unwrap().to_string();
    (app, responses, session_id)
}

/// #899: REST 経路（`record_rest_agent_reply`）は engine 生応答をそのまま `responses` に返し、
/// `speech` として保存していた。`NO_REPLY` のみの応答が `assistant: 'NO_REPLY'` として保存され、
/// 次ターンの typed 履歴でモデルへ渡る。配送層 3 箇所と同じ `terminate_at_no_reply` を保存/返却前に
/// 通し、沈黙は残さない。
///
/// 期待（テンプレ §1・観測境界＝REST の responses 件数/本文・memory_sessions 保存件数/本文・typed 履歴）:
///
/// | シナリオ            | responses 件数/本文 | agent speech 保存        | typed の assistant |
/// |---------------------|---------------------|--------------------------|--------------------|
/// | (a) `NO_REPLY` のみ | 0 件                | 0（NO_REPLY 行を残さない）| NO_REPLY 無し      |
/// | (b) 本文+`NO_REPLY` | 1 件・本文のみ      | 1（本文のみ・NO_REPLY 無）| 本文のみ           |
/// | (c) 対照: 通常応答  | 1 件                | 1                        | 通常応答           |
///
/// (a)(b) が現 tip で赤（生応答が返却・保存される）。
#[tokio::test]
async fn test_no_reply_only_is_not_persisted_rest_899() {
    const BODY_B: &str = "NR899B-本文だけ残る";
    const CTRL_C: &str = "NR899C-通常応答";
    const USER: &str = "u899";

    let (app, db, mock) = create_test_app_with_llm();
    let (agent_id, app) = create_test_agent(app).await;

    // FIFO: (a) 単独 NO_REPLY → (b) 本文+NO_REPLY → (d) NO_REPLY+CONTINUE → (c) 対照。
    mock.push_text_response("NO_REPLY");
    mock.push_text_response(&format!("{BODY_B}\nNO_REPLY"));
    mock.push_text_response("NO_REPLY\nCONTINUE");
    mock.push_text_response(CTRL_C);

    // (a) 単独 NO_REPLY: responses 0 件（§13 #11 / ターン合計 noreply）。
    let llm_before = mock.system_prompts().len();
    let (app, resp_a, session_id) = agent_message(app, &agent_id, USER, "問い a").await;
    assert_eq!(
        resp_a.len(),
        0,
        "(a) NO_REPLY のみなのに responses が返っている（#899）: {:?}",
        resp_a
    );
    // ターン合計 noreply: 沈黙でも生成は 1 回きり（LLM==1）。
    assert_eq!(
        mock.system_prompts().len() - llm_before,
        1,
        "(a) NO_REPLY のみのターンで LLM 呼び出しが 1 回でない（ターン合計 noreply: LLM==1）"
    );

    // (b) 本文+NO_REPLY: responses 1 件・本文のみ。
    let (app, resp_b, _) = agent_message(app, &agent_id, USER, "問い b").await;
    assert_eq!(resp_b.len(), 1, "(b) responses が 1 件でない: {:?}", resp_b);
    assert_eq!(
        resp_b[0]["content"].as_str().unwrap(),
        BODY_B,
        "(b) responses 本文が剥がし済みでない（#899）: {:?}",
        resp_b
    );

    // (d) NO_REPLY+CONTINUE: NO_REPLY 優先で沈黙。responses 0 件（§13 #13）。
    let (app, resp_d, _) = agent_message(app, &agent_id, USER, "問い d").await;
    assert_eq!(
        resp_d.len(),
        0,
        "(d) NO_REPLY+CONTINUE で NO_REPLY 優先の沈黙にならない（§13 #13）: {:?}",
        resp_d
    );

    // (c) 対照: responses 1 件・そのまま。
    let (_app, resp_c, _) = agent_message(app, &agent_id, USER, "問い c").await;
    assert_eq!(resp_c.len(), 1, "(c) responses が 1 件でない: {:?}", resp_c);
    assert_eq!(resp_c[0]["content"].as_str().unwrap(), CTRL_C);

    // --- 観測2: memory_sessions の agent speech 保存 ---
    let agent_speech: Vec<String> = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| {
                l.log_type == "speech" && l.speaker_id.as_deref() == Some(agent_id.as_str())
            })
            .map(|l| l.content)
            .collect()
    };
    assert_eq!(
        agent_speech
            .iter()
            .filter(|c| c.contains("NO_REPLY"))
            .count(),
        0,
        "NO_REPLY を含む agent speech 行が残っている（#899）: {:?}",
        agent_speech
    );
    let b_rows: Vec<&String> = agent_speech.iter().filter(|c| c.contains(BODY_B)).collect();
    assert_eq!(
        b_rows.len(),
        1,
        "(b) 本文の保存が 1 行でない: {:?}",
        agent_speech
    );
    assert!(
        !b_rows[0].contains("NO_REPLY"),
        "(b) 保存本文に NO_REPLY 混入: {:?}",
        b_rows[0]
    );
    assert_eq!(
        agent_speech.iter().filter(|c| c.contains(CTRL_C)).count(),
        1,
        "(c) 対照の保存が 1 行でない: {:?}",
        agent_speech
    );

    // --- 観測3: typed 履歴に assistant 'NO_REPLY' が無い ---
    let history = {
        let conn = db.lock().unwrap();
        opencrab_core::conversation_typed::build_typed_conversation(
            &conn,
            &session_id,
            &agent_id,
            200_000,
            100_000,
            false,
            false,
        )
        .unwrap()
        .history
    };
    let assistant_no_reply = history.iter().any(|m| {
        m.role == Role::Assistant
            && m.text_content()
                .map(|t| t.trim() == "NO_REPLY")
                .unwrap_or(false)
    });
    assert!(
        !assistant_no_reply,
        "typed 履歴に assistant 'NO_REPLY' が現れた（#899）: {:?}",
        history
            .iter()
            .map(|m| (
                format!("{:?}", m.role),
                m.text_content().map(|s| s.to_string())
            ))
            .collect::<Vec<_>>()
    );
}

/// #899 回帰: content 空＋非発話 tool_call の生成で、on_tool_call が空 speech 行を保存しない。
///
/// `visible_speech_after_markers("")` は `Some("")` を返すため、終端解釈の後に空/空白を弾く
/// ガードが要る（旧 `!content.trim().is_empty()` と同じ）。空 speech 行は typed 履歴に空の
/// assistant を生む。現 tip（本ブランチ実装後）で赤。
#[tokio::test]
async fn test_tool_only_generation_saves_no_empty_speech_899() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "ToolOnly", "TestPersona").await;

    // 1 巡目: content 空の非発話 tool_call。2 巡目: 通常テキストで締める。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-empty-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "x",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("完了しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "スキルを覚えて", "user_id": "u9"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "messages 200 でない: {resp}");
    let session_id = resp["session_id"].as_str().unwrap().to_string();

    // #899 回帰: 空/空白の agent speech 行を保存しない。
    let empty_speech = session_logs(&db, &session_id)
        .into_iter()
        .filter(|l| {
            l.log_type == "speech"
                && l.speaker_id.as_deref() == Some(agent_id.as_str())
                && l.content.trim().is_empty()
        })
        .count();
    assert_eq!(
        empty_speech, 0,
        "ツールのみ生成（content 空）で空の agent speech 行が保存された（#899 回帰）"
    );

    // 次ターンの typed 履歴に空の assistant が無い。
    let history = {
        let conn = db.lock().unwrap();
        opencrab_core::conversation_typed::build_typed_conversation(
            &conn,
            &session_id,
            &agent_id,
            200_000,
            100_000,
            false,
            false,
        )
        .unwrap()
        .history
    };
    let empty_assistant = history.iter().any(|m| {
        m.role == Role::Assistant
            && m.text_content()
                .map(|t| t.trim().is_empty())
                .unwrap_or(false)
    });
    assert!(
        !empty_assistant,
        "typed 履歴に空の assistant が現れた（#899 回帰）"
    );
}

/// #899 ガードの真の穴埋め（テストレビュー所見・非回帰）: **on_tool_call 経路**で content が
/// **"NO_REPLY"（非空テキスト）** ＋ 照会/道具 tool_call を 1 生成で併記したとき、保存前の
/// NO_REPLY 終端解釈（`visible_speech_after_markers`・#903 §12.6）が効き "NO_REPLY" speech を
/// 残さない・次ターン typed 履歴にも現れないことを固定する。
///
/// 既存 `test_tool_only_generation_saves_no_empty_speech_899` は content=**空**のみを通すため、
/// 旧 `!content.trim().is_empty()` filter でも通り、#903 の NO_REPLY 対応を区別しない（恒真）。
/// 本テストは content="NO_REPLY"（非空）を通すので、ガードを旧 empty-only filter へ revert すると
/// "NO_REPLY" が speech 保存され**赤になる**（現 tip では #903 ガードが効き緑）。
#[tokio::test]
async fn test_no_reply_text_with_tool_call_saves_no_speech_899_guard() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "NoReplyTool", "TestPersona").await;

    // 1 巡目: content="NO_REPLY"（非空）＋ 非発話 tool_call を同時に返す（on_tool_call が
    // content="NO_REPLY" で発火する）。2 巡目: 通常テキストで締める。
    mock.push_text_and_tool_call_response(
        "NO_REPLY",
        vec![ToolCall {
            id: "tc-nr-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "learn_from_experience".to_string(),
                arguments: serde_json::json!({
                    "skill_name": "x",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "g"
                })
                .to_string(),
            },
        }],
    );
    mock.push_text_response("完了しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "覚えて", "user_id": "u10"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "messages 200 でない: {resp}");
    let session_id = resp["session_id"].as_str().unwrap().to_string();

    // on_tool_call 経路: content="NO_REPLY" の speech 行を残さない（#903 ガード）。
    let no_reply_speech = session_logs(&db, &session_id)
        .into_iter()
        .filter(|l| {
            l.log_type == "speech"
                && l.speaker_id.as_deref() == Some(agent_id.as_str())
                && l.content.contains("NO_REPLY")
        })
        .count();
    assert_eq!(
        no_reply_speech, 0,
        "on_tool_call 経路で content=NO_REPLY が speech 保存された（#899 ガード revert）"
    );

    // 次ターンの typed 履歴に assistant 'NO_REPLY' が無い。
    let history = {
        let conn = db.lock().unwrap();
        opencrab_core::conversation_typed::build_typed_conversation(
            &conn,
            &session_id,
            &agent_id,
            200_000,
            100_000,
            false,
            false,
        )
        .unwrap()
        .history
    };
    let has_no_reply_assistant = history.iter().any(|m| {
        m.role == Role::Assistant
            && m.text_content()
                .map(|t| t.contains("NO_REPLY"))
                .unwrap_or(false)
    });
    assert!(
        !has_no_reply_assistant,
        "typed 履歴に assistant 'NO_REPLY' が現れた（#899 ガード revert）"
    );
}

