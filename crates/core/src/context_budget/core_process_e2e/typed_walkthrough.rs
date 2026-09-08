// ==================== #884 PR2: typed 送信の回帰ハーネス + モデル視点ウォークスルー ====================

/// ウォークスルー成果物の出力先。**env `OC_WALKTHROUGH_DIR` が設定されたときだけ**書き出す
/// （CI/他環境ではハードコードパスへ書かず no-op・テストの assert はファイル書き込みに依存しない）。
const WALKTHROUGH_DIR_ENV: &str = "OC_WALKTHROUGH_DIR";

/// 送信 messages を捕捉し LLM 呼び出し回数を数える mock。tool を出さない最終テキストを
/// 返すので engine は 1 回で止まる（#885 の chat カウンタ流儀を core で再利用）。
struct CapturingLlm {
    captured: std::sync::Arc<std::sync::Mutex<Vec<Message>>>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reply: String,
}

#[async_trait]
impl LlmClient for CapturingLlm {
    async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        {
            let mut guard = self.captured.lock().unwrap();
            if guard.is_empty() {
                *guard = request.messages.clone();
            }
        }
        Ok(text_resp(&self.reply))
    }
}

fn insert_log(
    conn: &rusqlite::Connection,
    log_type: &str,
    speaker: Option<&str>,
    content: &str,
    meta: Option<serde_json::Value>,
    created_at: &str,
) {
    let row = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: log_type.into(),
        content: content.into(),
        speaker_id: speaker.map(str::to_owned),
        turn_number: None,
        metadata_json: meta.map(|value| value.to_string()),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log_at(conn, &row, created_at).unwrap();
}

/// sleep-60（execute_shell の settle→resume）の実ログ列を seed する（設計 §6.1/§6.2 に対応）。
fn seed_sleep60(conn: &rusqlite::Connection) {
    insert_log(
        conn,
        "speech",
        Some("u-9"),
        "くらぶ、60秒sleepして終わったら教えて",
        None,
        "2026-09-01T16:16:41+00:00",
    );
    let call_meta = serde_json::json!({
        "tool_calls_json": serde_json::json!([{
            "id": "call_c253",
            "type": "function",
            "function": {
                "name": "execute_shell",
                "arguments": "{\"args\":[\"60\"],\"command\":\"sleep\",\"stdin\":\"\",\"timeout_secs\":90}"
            }
        }])
        .to_string()
    });
    insert_log(
        conn,
        "tool_call",
        Some(AGENT),
        "execute_shell",
        Some(call_meta),
        "2026-09-01T16:16:49+00:00",
    );
    insert_log(
        conn,
        "tool_result",
        Some(AGENT),
        r#"{"status":"spawned","subtask_id":"s76","tool":"execute_shell","tool_call_id":"call_c253"}"#,
        Some(serde_json::json!({"tool_call_id": "call_c253", "tool_name": "execute_shell"})),
        "2026-09-01T16:16:49+00:00",
    );
    let completed = serde_json::json!({
        "type": "subtask_completed",
        "subtask_id": "s76",
        "session_id": "sub-1",
        "exit_reason": "completed",
        "result": serde_json::json!({"success": true, "data": {"exit_code": 0, "stdout": "", "stderr": ""}})
            .to_string()
    });
    insert_log(
        conn,
        "system",
        None,
        &completed.to_string(),
        None,
        "2026-09-01T16:17:49+00:00",
    );
}

fn write_walkthrough(name: &str, value: &serde_json::Value) {
    // env が無ければ何もしない（CI・他環境で panic しない・best-effort）。
    let Ok(dir) = std::env::var(WALKTHROUGH_DIR_ENV) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(
        format!("{dir}/{name}"),
        serde_json::to_string_pretty(value).unwrap_or_default(),
    );
}

async fn capture_engine_messages(
    typed: Option<crate::conversation_typed::TypedConversation>,
    user_message: &str,
) -> (Vec<Message>, usize) {
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let llm = CapturingLlm {
        captured: captured.clone(),
        calls: calls.clone(),
        reply: "ok".into(),
    };
    let mut engine = SkillEngine::new(Box::new(llm), Box::new(LoopingExecutor), 3);
    engine.set_conversation_waters(HIGH, LOW);
    engine.set_typed_conversation(typed);
    engine
        .run("system context", user_message, "mock")
        .await
        .unwrap();
    let messages = captured.lock().unwrap().clone();
    let count = calls.load(std::sync::atomic::Ordering::SeqCst);
    (messages, count)
}

/// §8.2（PR2 分）の回帰固定 + §6 ウォークスルー成果物（sleep-60 の flag off/on 実 messages）。
#[tokio::test]
async fn typed_send_regression_sleep60() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_sleep60(&conn);

    // flag off（flat）: 既存挙動を完全維持（System + 単一 User）。
    let flat =
        build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false).unwrap();
    let (off_msgs, off_calls) = capture_engine_messages(None, &flat).await;
    assert_eq!(off_msgs.len(), 2, "flat は System + 単一 User");
    assert_eq!(off_msgs[0].role, Role::System);
    assert_eq!(off_msgs[1].role, Role::User);

    // flag on（typed）。
    let tc = crate::conversation_typed::build_typed_conversation(
        &conn, SESSION, AGENT, HIGH, LOW, false, true,
    )
    .unwrap();
    let (on_msgs, on_calls) = capture_engine_messages(Some(tc), "(ignored in typed)").await;

    assert_eq!(on_msgs[0].role, Role::System);
    // 症状 B 根治: settle 後も sleep 引数が assistant.tool_calls に見える。
    let has_args = on_msgs.iter().any(|m| {
        m.role == Role::Assistant
            && m.tool_calls.as_ref().is_some_and(|calls| {
                calls.iter().any(|c| {
                    c.function.arguments.contains("sleep") && c.function.arguments.contains("60")
                })
            })
    });
    assert!(
        has_args,
        "typed: assistant.tool_calls に sleep 引数が見える"
    );
    // 結果は role=Tool。
    assert!(
        on_msgs.iter().any(|m| m.role == Role::Tool),
        "typed: role=Tool の結果がある"
    );
    // →log: の裸参照が消える。
    let on_json = serde_json::to_string(&on_msgs).unwrap();
    assert!(!on_json.contains("→log:"), "typed: →log: の裸参照が無い");
    // #885 再利用: typed 化で LLM 呼び出し回数は変わらない。
    assert_eq!(off_calls, on_calls, "typed 化で LLM 呼び出し回数不変");
    assert_eq!(on_calls, 1);

    write_walkthrough(
        "sleep60-flagoff.json",
        &serde_json::to_value(&off_msgs).unwrap(),
    );
    write_walkthrough(
        "sleep60-flagon.json",
        &serde_json::to_value(&on_msgs).unwrap(),
    );
}

fn dump_typed(
    conn: &rusqlite::Connection,
    name: &str,
) -> crate::conversation_typed::TypedConversation {
    let tc = crate::conversation_typed::build_typed_conversation(
        conn, SESSION, AGENT, HIGH, LOW, false, true,
    )
    .unwrap();
    let mut msgs = Vec::new();
    if let Some(c) = &tc.context_block {
        msgs.push(c.clone());
    }
    if let Some(s) = &tc.snapshot_base {
        msgs.push(s.clone());
    }
    msgs.extend(tc.history.iter().cloned());
    let obj = serde_json::json!({
        "messages": msgs,
        "response_directive": tc.response_directive,
        "wire_tokens": tc.wire_tokens,
    });
    write_walkthrough(name, &obj);
    tc
}

/// QC 想定シナリオ（reply×3・NO_REPLY）の flag on messages をウォークスルーへ書き出す。
#[test]
fn typed_walkthrough_reply3_and_noreply() {
    // reply×3: user/agent 交互。
    let conn = opencrab_db::init_memory().unwrap();
    insert_log(
        &conn,
        "speech",
        Some("u-9"),
        "1件目お願い",
        None,
        "2026-09-01T10:00:00+00:00",
    );
    insert_log(
        &conn,
        "speech",
        Some(AGENT),
        "1件目やりました",
        None,
        "2026-09-01T10:00:05+00:00",
    );
    insert_log(
        &conn,
        "speech",
        Some("u-9"),
        "2件目お願い",
        None,
        "2026-09-01T10:01:00+00:00",
    );
    insert_log(
        &conn,
        "speech",
        Some(AGENT),
        "2件目やりました",
        None,
        "2026-09-01T10:01:05+00:00",
    );
    insert_log(
        &conn,
        "speech",
        Some("u-9"),
        "3件目お願い",
        None,
        "2026-09-01T10:02:00+00:00",
    );
    insert_log(
        &conn,
        "speech",
        Some(AGENT),
        "3件目やりました",
        None,
        "2026-09-01T10:02:05+00:00",
    );
    let tc = dump_typed(&conn, "reply3-flagon.json");
    let users = tc.history.iter().filter(|m| m.role == Role::User).count();
    let assistants = tc
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    assert!(
        users >= 3 && assistants >= 3,
        "reply×3: User/Assistant が各3以上"
    );

    // NO_REPLY: user 発話のみ（agent 応答なし）。
    let conn2 = opencrab_db::init_memory().unwrap();
    insert_log(
        &conn2,
        "speech",
        Some("u-9"),
        "これ見て（返信不要）",
        None,
        "2026-09-01T11:00:00+00:00",
    );
    insert_log(
        &conn2,
        "speech",
        Some("u-7"),
        "私も見た",
        None,
        "2026-09-01T11:00:10+00:00",
    );
    let tc2 = dump_typed(&conn2, "noreply-flagon.json");
    assert!(
        tc2.history.iter().all(|m| m.role == Role::User),
        "NO_REPLY: history は User のみ"
    );
    assert!(!tc2.history.iter().any(|m| m.role == Role::Assistant));
}

/// row368 の負の対照: flat 経路（flag off）は typed の判別基準を満たさない
/// ——単一 User に tool_calls が無く、`→log:` の裸参照が残る。typed 検査が判別力を
/// 持つこと（＝赤 demo の意味）を CI 上に恒久的に残す対照テスト。
#[tokio::test]
async fn flat_path_lacks_typed_structure_negative_control() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_sleep60(&conn);
    let flat =
        build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false).unwrap();
    let (msgs, _calls) = capture_engine_messages(None, &flat).await;
    // flat は System + 単一 User の 2 本。
    assert_eq!(msgs.len(), 2, "flat は System + 単一 User");
    assert_eq!(msgs[1].role, Role::User);
    // typed の判別基準（assistant.tool_calls）を満たさない。
    assert!(
        !msgs.iter().any(|m| m.tool_calls.is_some()),
        "flat には tool_calls が無い"
    );
    // 症状 B: settle 後の引数は →log: の裸参照へ潰れている。
    let json = serde_json::to_string(&msgs).unwrap();
    assert!(json.contains("→log:"), "flat には →log: の裸参照が残る");
}
