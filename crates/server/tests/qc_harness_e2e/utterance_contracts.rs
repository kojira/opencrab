// ==================== (A3) 発話クラス reply: 撃ちっぱなし（第三柱・§3.3.1） ====================
//
// DESIGN-RESUME-SETTLE §6 A3: reply/reaction は (i) subtask/settle/resume を起こさない・
// (ii) 会話ログに機械行（tool_call/tool_result/sN）を残さない（本文＋関係注記のみ）・
// (iii) 配送される（dry-run に出る）。実配線（実 extgate + 実 nostr-gateway dry-run invoke）を通す。

const M_A3: &str = "A3REPLY-MARK";
const B_A3: &str = "A3-返信本文だよ";
const M_A3_THREE: &str = "A3-THREE-REPLIES-MARK";
const B_A3_THREE: [&str; 3] = ["A3-返信その1", "A3-返信その2", "A3-返信その3"];

struct A3Mock {
    chat_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for A3Mock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        let text = request_text(&request);
        // 発話 reply の最小 ack（tool role）を受けたら沈黙で閉じる（resume は起きない）。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        if text.contains(M_A3_THREE) {
            let mut response = tool_calls_response(
                B_A3_THREE
                    .iter()
                    .map(|body| ("reply", serde_json::json!({"event": "e1", "text": body})))
                    .collect(),
            );
            response.choices[0].message.content =
                Some(MessageContent::Text("NO_REPLY".to_string()));
            return Ok(response);
        }
        if text.contains(M_A3) {
            return Ok(reply_with_content_response(B_A3, "NO_REPLY"));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn scenario_a3_reply_utterance_no_subtask_no_machine_lines() {
    let buf = install_capture();
    let mock = Arc::new(A3Mock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a3".repeat(32);
    fixture.append_line(&mention_event(&ev, &format!("{M_A3} これに返信して")));

    // (iii) reply が配送される（dry-run に kind="reply" body が出る）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_A3))
        })
        .await
    };
    assert!(delivered, "発話 reply が配送されない: {:?}", captured(&buf));

    // (i) subtask/settle/resume が起きない: reply は inline 発話なので背景 subtask に載らない。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "発話 reply が subtask 化された（撃ちっぱなしでない）"
    );
    // reply は 1 回だけ（resume 復唱・自動再送ゼロ）。
    let reply_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.body.contains(B_A3))
        .count();
    assert_eq!(
        reply_count,
        1,
        "reply が複数回配送された（復唱）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        1,
        "発話 reply は最小 ack 往復を起こさず 1 生成で完了する"
    );

    // (ii) 機械行を残さない: session_logs に reply の tool_call/tool_result が無く、本文は speech で残る。
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    let kinds: Vec<(&str, &str)> = logs
        .iter()
        .map(|l| (l.log_type.as_str(), l.content.as_str()))
        .collect();
    assert!(
        !logs
            .iter()
            .any(|l| l.log_type == "tool_call" || l.log_type == "tool_result"),
        "発話 reply が機械行(tool_call/tool_result)を残した: {kinds:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.log_type == "speech" && l.content.contains(B_A3)),
        "reply 本文が speech として残っていない: {kinds:?}"
    );
}

/// #880: reply×3 を 1 生成に並べ、3 通を配送して LLM 往復なしで完了する。
#[tokio::test]
async fn scenario_a3_three_replies_complete_in_one_llm_call_without_subtask() {
    let buf = install_capture();
    let mock = Arc::new(A3Mock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a4".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        &format!("{M_A3_THREE} 3回に分けて返信して"),
    ));

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_A3_THREE.iter().all(|body| {
                captured(&buf)
                    .iter()
                    .any(|captured| captured.kind == "reply" && captured.body.contains(body))
            })
        })
        .await
    };
    assert!(
        delivered,
        "1 生成の reply×3 がすべて配送されない: {:?}",
        captured(&buf)
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    for body in B_A3_THREE {
        let count = captured(&buf)
            .iter()
            .filter(|captured| captured.kind == "reply" && captured.body.contains(body))
            .count();
        assert_eq!(count, 1, "reply 本文 {body} の配送回数が 1 でない");
    }
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        1,
        "reply×3 は ack ごとの LLM 再呼び出しを起こさない"
    );
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "reply×3 が subtask 化された"
    );
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        !logs
            .iter()
            .any(|log| log.log_type == "tool_call" || log.log_type == "tool_result"),
        "reply×3 が機械行を残した: {:?}",
        logs.iter()
            .map(|log| (&log.log_type, &log.content))
            .collect::<Vec<_>>()
    );
    // §13 ターン合計 reply3-in-one: 保存 3（memory_sessions の agent 発話 speech 行が 3）。
    // speaker_id==AGENT_ID で発端メッセージ（inbound・speaker=送信者）を除いて数える。
    let agent_speech_saves = logs
        .iter()
        .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
        .count();
    assert_eq!(
        agent_speech_saves,
        3,
        "reply×3 in one の agent 発話 speech 保存が 3 でない（§13 reply3-in-one=保存3）: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.speaker_id, &l.content))
            .collect::<Vec<_>>()
    );
}

// ==================== (A3-継続) 発話のみ＋末尾 継続 で継続（#900） ====================
//
// #900: reply（発話クラス）のみの生成でも content 末尾が 継続 単独なら、発話を配送してから
// 次イテレーションへ進む。reply×1＋継続 を 3 回連ねると、3 通配送・LLM 3 呼び出し・継続 は
// 本文へ残らない（撃ちっぱなし＋末尾マーカーの併記契約）。旧挙動（純発話は 1 生成で必ず完結）だと
// 1 通・LLM 1 で止まるので、この差が回帰ガードになる。

// マーカー文字列自体に "継続" を含めない（残留検査で発端メッセージが誤検知するのを避ける）。
const M_A3_CONT: &str = "A3CONTMARK";
const B_A3_CONT: [&str; 3] = ["A3-CONT返信1", "A3-CONT返信2", "A3-CONT返信3"];

/// reply tool_call と content を同一生成に載せる（tool_calls_response は content=None のため）。
fn reply_with_content_response(text: &str, content: &str) -> ChatResponse {
    let mut resp = tool_call_response("reply", serde_json::json!({"event": "e1", "text": text}));
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

struct A3ContinueMock {
    chat_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for A3ContinueMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        // 生成回数で分岐する（純発話＋継続 の継続は tool role ack を返すため、text マーカーだけ
        // では 1・2・3 回目を区別できない）。1・2 回目は reply＋末尾 継続、3 回目は reply のみ。
        let n = self.chat_calls.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            Ok(reply_with_content_response(B_A3_CONT[n], ""))
        } else {
            Ok(reply_with_content_response(B_A3_CONT[2], "NO_REPLY"))
        }
    }
}

/// #900: reply×1＋末尾 継続 を 3 回連ねる → 3 通配送・LLM 3 呼び出し・継続 非残留。
#[tokio::test]
async fn scenario_a3_utterance_only_with_continue_runs_next_iteration() {
    let buf = install_capture();
    let mock = Arc::new(A3ContinueMock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a5".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        &format!("{M_A3_CONT} 3回に分けて返信して"),
    ));

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_A3_CONT.iter().all(|body| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(body))
            })
        })
        .await
    };
    assert!(
        delivered,
        "reply×1＋継続 の 3 連が全通配送されない（継続が起きていない）: {:?}",
        captured(&buf)
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    for body in B_A3_CONT {
        let count = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(body))
            .count();
        assert_eq!(count, 1, "reply 本文 {body} の配送回数が 1 でない");
    }
    // 3 回の生成すべてが走る（1・2 回目は 継続 で継続、3 回目で自然終了）。
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        3,
        "純発話＋末尾 継続 が次イテレーションを起こさない（1 生成で止まった）"
    );
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "発話＋継続 が subtask 化された"
    );
    // 継続 は say としても speech ログとしても残らない（剥がされて空になる）。
    let no_continue_captured = captured(&buf).iter().all(|c| !c.body.contains("継続"));
    assert!(
        no_continue_captured,
        "配送本文に 継続 が残留: {:?}",
        captured(&buf)
    );
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        !logs.iter().any(|l| l.content.contains("継続")),
        "session_logs に 継続 が残留: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.content))
            .collect::<Vec<_>>()
    );
    // §13 ターン合計 reply1＋継続×2: 保存 3（各イテレーションの reply が speech 保存される）。
    let agent_speech_saves = logs
        .iter()
        .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
        .count();
    assert_eq!(
        agent_speech_saves, 3,
        "reply1＋継続×2 の agent 発話 speech 保存が 3 でない（§13=保存3・§12.2 各イテレーション保存）: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.speaker_id, &l.content))
            .collect::<Vec<_>>()
    );
}

// ==================== (N2) 照会クラス resolve: 従来どおり subtask 化（非回帰・§6 N2） ====================
//
// resolve は結果を読む照会クラス。発話クラス化に巻き込まれず、従来どおり Dispatchable →
// 背景 subtask → 機械行（tool_call）を残す。A3（発話）との構造対比で「照会は殺していない」を固定。
// settle→resume が結果を読む経路自体は scenario_main_second_request_not_blocked_during_long_op
// （spawn_subtask の settle→resume→完了 say）が別途固定している。

const M_N2: &str = "N2RESOLVE-MARK";

struct N2Mock;

#[async_trait::async_trait]
impl LlmProvider for N2Mock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        let text = request_text(&request);
        if has_tool_role(&request) {
            // spawned ack / resume は沈黙で閉じる（本テストは分類=照会の構造だけを見る）。
            return Ok(text_response("NO_REPLY"));
        }
        if text.contains(M_N2) {
            return Ok(tool_call_response(
                "resolve",
                serde_json::json!({"ref": "e1"}),
            ));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn scenario_n2_resolve_query_class_keeps_subtask_and_machine_line() {
    let buf = install_capture();
    let mock = Arc::new(N2Mock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "c2".repeat(32);
    fixture.append_line(&mention_event(&ev, &format!("{M_N2} これの全文を見て")));

    // 照会 resolve は Dispatchable → tool_call 機械行が残る（発話クラスと異なり撃ちっぱなしでない）。
    let saw_resolve_call = {
        let core = &core;
        let session_id = session_id.clone();
        wait_until(move || {
            let conn = core.extgate.db.lock().unwrap();
            let logs =
                opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap();
            // resolve の名は tool_call ログの metadata（tool_calls_json）に載る（content は空）。
            logs.iter().any(|l| {
                l.log_type == "tool_call"
                    && l.metadata_json
                        .as_deref()
                        .is_some_and(|m| m.contains("resolve"))
            })
        })
        .await
    };
    // resolve が発話クラスに誤分類されていれば invoke_utterance 経由になり tool_call 機械行を
    // 残さない。tool_call ログの存在が「照会クラス（Dispatchable・subtask）のまま」を証す。
    // （dry-run 配送 buffer は全テスト共有でここでは判定に使わない。）
    assert!(
        saw_resolve_call,
        "resolve の tool_call 機械行が残らない（照会が発話クラスに誤分類された）"
    );
    let _ = &buf;
}

