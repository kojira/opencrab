#[test]
fn results_bind_only_to_recorded_call_ids() {
    let batch = derive(&[
        row(
            1,
            "tool_call",
            Some(AGENT),
            "batch",
            Some(tool_calls_metadata(json!([
                call("call_a", "tool_a", json!({"slot": "a"})),
                call("call_b", "tool_b", json!({"slot": "b"})),
            ]))),
        ),
        row(
            2,
            "tool_result",
            Some(AGENT),
            r#"{"success":true,"data":{"exit_code":0,"stdout":"a","stderr":""}}"#,
            Some(json!({"tool_call_id": "call_a", "tool_name": "tool_a"})),
        ),
        row(
            3,
            "tool_result",
            Some(AGENT),
            r#"{"success":true,"data":{"exit_code":0,"stdout":"b","stderr":""}}"#,
            Some(json!({"tool_call_id": "call_b", "tool_name": "tool_b"})),
        ),
    ]);
    let result_a = batch
        .items
        .iter()
        .find(|item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_a"));
    assert!(matches!(
        result_a,
        Some(TypedItem::ToolResult {
            tool_name,
            body: ResultBody::Inline(value),
            ..
        }) if tool_name == "tool_a" && value["stdout"] == "a"
    ));
    let result_b = batch
        .items
        .iter()
        .find(|item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_b"));
    assert!(matches!(
        result_b,
        Some(TypedItem::ToolResult {
            tool_name,
            body: ResultBody::Inline(value),
            ..
        }) if tool_name == "tool_b" && value["stdout"] == "b"
    ));

    let unknown = derive(&[
        row(
            10,
            "tool_call",
            Some(AGENT),
            "tool_x",
            Some(tool_calls_metadata(json!([call(
                "call_x",
                "tool_x",
                json!({}),
            )]))),
        ),
        row(
            11,
            "tool_result",
            Some(AGENT),
            r#"{"success":true}"#,
            Some(json!({
                "tool_call_id": "call_UNKNOWN",
                "tool_name": "tool_x",
            })),
        ),
    ]);
    assert!(unknown.items.iter().any(|item| matches!(
        item,
        TypedItem::MachineEvent {
            kind,
            related_call: Some(call_id),
            opaque: true,
            ..
        } if kind == "tool_result" && call_id == "call_UNKNOWN"
    )));
    assert!(unknown.diagnostics.opaque_event_count >= 1);
    assert!(unknown.diagnostics.unpaired_call_count >= 1);

    let cancelled = derive(&[
        row(
            20,
            "tool_call",
            Some(AGENT),
            "tool_c",
            Some(tool_calls_metadata(json!([call(
                "call_c",
                "tool_c",
                json!({}),
            )]))),
        ),
        row(
            21,
            "tool_cancelled",
            Some(AGENT),
            r#"{"reason":"cancelled"}"#,
            Some(json!({"tool_call_id": "call_c", "tool_name": "tool_c"})),
        ),
    ]);
    assert!(cancelled.items.iter().any(|item| matches!(
        item,
        TypedItem::ToolCall {
            call_id,
            state: ToolCallState::Cancelled,
            ..
        } if call_id == "call_c"
    )));
    assert!(cancelled.items.iter().any(|item| matches!(
        item,
        TypedItem::ToolResult {
            call_id,
            state: ToolResultState::Cancelled,
            ..
        } if call_id == "call_c"
    )));
}

// secret トリップワイヤ（§9.2-4）。
//
// **これは redaction の検査ではない。** derive_items は arguments/結果を verbatim 保持する
// 仕様（症状 B の修正そのもの＝引数を潰さない）で、ログ行に秘密値が書かれていれば derive は
// そのまま出す。したがってここが守るのは 1 点だけ:
// **derive_items は入力ログ行に無い値（プロセス env・周辺状態）を出力 item に混入させない。**
// 秘密が env にだけあり memory_sessions のどの行にも書かれていなければ、typed item にも出ない。
//
// §9.2-4 の本体「env 注入値が memory_sessions.tool_calls_json に一切書かれない」は tool_call を
// 記録する保存層（server/process.rs 等の write 経路）の責務で、derive スコープ外
// （PR1 は保存層を変更しないため未検査。保存層トリップワイヤは別 PR で起票する）。
//
// 恒真化を防ぐため 2 方向で固定する:
// - 正の対照: 秘密値を **明示的に埋めた** ログ行では derive が verbatim に出す（＝走査が本当に
//   秘密値を検出できることの証明。走査や対照が壊れていればここで落ちる）。
// - 本検査: 秘密値をどの行にも入れず（引数は $OC_TEST_SECRET と名前で参照）env だけに置くと、
//   arguments・content・Omission・診断のどこにも秘密値は出ない。
#[test]
fn env_injected_secret_not_pulled_into_items() {
    const SECRET: &str = "S3CRET-DO-NOT-LEAK-abcdef";
    std::env::set_var("OC_TEST_SECRET", SECRET);

    // 正の対照: 秘密値をログ本文へ実際に埋めると、verbatim 保持で必ず出る（走査の健全性）。
    let planted = derive(&[row(
        1,
        "tool_call",
        Some(AGENT),
        "execute_shell",
        Some(tool_calls_metadata(json!([call(
            "call_planted",
            "execute_shell",
            json!({"command": "echo", "args": [SECRET]}),
        )]))),
    )]);
    let planted_json = serde_json::to_string(&planted.items).unwrap();
    assert!(
        planted_json.contains(SECRET),
        "対照: 本文へ埋めた秘密値は verbatim 保持で出るはず（出ないなら走査/対照が壊れている）"
    );

    // 本検査: どのログ行にも秘密値を入れず、引数は env を名前で参照するだけにする。
    let clean = derive(&[
        row(
            10,
            "speech",
            Some(USER),
            "秘密は $OC_TEST_SECRET を使って",
            None,
        ),
        row(
            11,
            "tool_call",
            Some(AGENT),
            "execute_shell",
            Some(tool_calls_metadata(json!([call(
                "call_ref",
                "execute_shell",
                // 値ではなく env 変数名を参照する（設計の正本＝秘密は env 注入のみ）。
                json!({"command": "sh", "args": ["-c", "echo $OC_TEST_SECRET"]}),
            )]))),
        ),
        row(
            12,
            "tool_result",
            Some(AGENT),
            // 大きな read 結果 → Omission。pointer/診断まで走査対象に含める。
            &json!({
                "success": true,
                "data": {
                    "path": "/workspace/report.txt",
                    "content": "x".repeat(80_000),
                    "has_more": true,
                },
            })
            .to_string(),
            Some(json!({"tool_call_id": "call_ref", "tool_name": "execute_shell"})),
        ),
    ]);
    // arguments・content・Omission の pointer・診断文字列まで、全 item を 1 本の JSON にして走査。
    let mut scanned = serde_json::to_string(&clean.items).unwrap();
    scanned.push_str(&serde_json::to_string(&clean.diagnostics).unwrap());
    let leaked = scanned.contains(SECRET);
    std::env::remove_var("OC_TEST_SECRET");
    assert!(
        !leaked,
        "derive が env/周辺状態から秘密値を取り込んではならない（引数は名前参照のみ）"
    );
}

// 大きな read 結果だけを説明文なしの構造化 omission にし、shell stdout は Inline に残す。
#[test]
fn large_read_result_becomes_structured_omission() {
    let large_content = "x".repeat(80_000);
    let derived = derive(&[
        row(
            1,
            "tool_call",
            Some(AGENT),
            "ws_read",
            Some(tool_calls_metadata(json!([call(
                "call_r",
                "ws_read",
                json!({"path": "/workspace/report.txt"}),
            )]))),
        ),
        row(
            2,
            "tool_result",
            Some(AGENT),
            &json!({
                "success": true,
                "data": {
                    "path": "/workspace/report.txt",
                    "content": large_content,
                    "has_more": true,
                },
            })
            .to_string(),
            Some(json!({"tool_call_id": "call_r", "tool_name": "ws_read"})),
        ),
        row(
            3,
            "tool_call",
            Some(AGENT),
            "execute_shell",
            Some(tool_calls_metadata(json!([call(
                "call_s",
                "execute_shell",
                json!({"command": "printf", "args": ["ok"]}),
            )]))),
        ),
        row(
            4,
            "tool_result",
            Some(AGENT),
            r#"{"success":true,"data":{"exit_code":0,"stdout":"ok","stderr":""}}"#,
            Some(json!({
                "tool_call_id": "call_s",
                "tool_name": "execute_shell",
            })),
        ),
    ]);
    let read_result = derived
        .items
        .iter()
        .find(|item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_r"));
    let Some(TypedItem::ToolResult { body, .. }) = read_result else {
        panic!("ws_read の ToolResult が存在すること");
    };
    let ResultBody::Omitted(omitted) = body else {
        panic!("大きな read 結果は Omitted であること");
    };
    assert_eq!(omitted.reason, "read_result_budget");
    assert_eq!(omitted.pointer.kind, "workspace_path");
    assert_eq!(
        omitted.pointer.path.as_deref(),
        Some("/workspace/report.txt")
    );
    assert!(omitted.resolvable);
    assert_eq!(omitted.tool.as_deref(), Some("ws_read"));
    let omission_json = serde_json::to_string(omitted).unwrap();
    assert!(!omission_json.contains("本文は会話に残していない"));
    assert!(!omission_json.contains("必要ならもう一度"));

    assert!(derived.items.iter().any(|item| matches!(
        item,
        TypedItem::ToolResult {
            call_id,
            body: ResultBody::Inline(value),
            ..
        } if call_id == "call_s" && value["stdout"] == "ok"
    )));
}

// DB 上の snapshot なしログ列でも shadow builder が最後まで疎通することを固定する。
#[test]
fn shadow_comparison_runs_over_db() {
    let conn = opencrab_db::init_memory().unwrap();
    for mut log in sleep_rows(true) {
        log.id = None;
        opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
    }
    let diagnostics = super::run_shadow_comparison(&conn, "sess-1", AGENT, 100_000, 50_000, false);
    assert!(diagnostics.item_count > 0);
}

#[test]
fn typed_conversation_no_snapshot_basic() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_sleep_session(&conn);

    let conversation =
        super::build_typed_conversation(&conn, "sess-1", AGENT, 100_000, 50_000, false, true)
            .unwrap();

    assert!(conversation.snapshot_base.is_none());
    let tool_call_message = conversation
        .history
        .iter()
        .find(|message| message.role == Role::Assistant && message.tool_calls.is_some())
        .expect("assistant tool call message");
    let wire = serde_json::to_string(tool_call_message).unwrap();
    assert!(wire.contains("sleep"));
    assert!(conversation
        .history
        .iter()
        .any(|message| message.role == Role::Tool));
    assert_eq!(
        conversation.response_directive.as_deref(),
        Some(crate::conversation::CONVERSATION_RESPONSE_GUIDANCE)
    );
    assert!(conversation.wire_tokens > 0);
}

#[test]
fn typed_conversation_directive_off() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_sleep_session(&conn);

    let conversation =
        super::build_typed_conversation(&conn, "sess-1", AGENT, 100_000, 50_000, false, false)
            .unwrap();

    assert!(conversation.response_directive.is_none());
}

#[test]
fn typed_conversation_empty_session() {
    let conn = opencrab_db::init_memory().unwrap();

    let conversation = super::build_typed_conversation(
        &conn,
        "empty-session",
        AGENT,
        100_000,
        50_000,
        false,
        true,
    )
    .unwrap();

    assert!(conversation.history.is_empty());
    assert!(conversation.snapshot_base.is_none());
    assert!(conversation.response_directive.is_none());
}
