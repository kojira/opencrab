// ---------------------------------------------------------------------------
// #899(a)【§13 #11（NO_REPLY のみ→配送0/保存0/DB にも書かない）nostr レーン・§13.1 i（typed on/off 同一期待）】:
// NO_REPLY のみの応答 → 配送 0・保存 0・次ターン typed 履歴に assistant NO_REPLY なし。
//
// 現 tip: delivery_effect は NoReply（配送 0）だが apply_delivery_effect が
// record_agent_no_reply を呼び "NO_REPLY" を speech 保存する → 次ターンの会話履歴に
// assistant "NO_REPLY" として載る（合言葉の模倣温床）。→ 保存/履歴の pin で赤。
// ---------------------------------------------------------------------------
const M899_1: &str = "NR899-MARK-ONE これは黙って";
const M899_2: &str = "NR899-MARK-TWO 次の依頼";
const B899_TURN2: &str = "nr899-turn2-body 二ターン目の返事";

struct NoReplyProbeMock {
    calls: AtomicUsize,
    turn2_assistant: Mutex<Option<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmProvider for NoReplyProbeMock {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = request_text(&request);
        if text.contains(M899_2) {
            // ターン 2: 履歴に載った Assistant ロールメッセージ本文を捕まえる。
            let asst: Vec<String> = request
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .filter_map(|m| m.text_content().map(|s| s.to_string()))
                .collect();
            *self.turn2_assistant.lock().unwrap() = Some(asst);
            return Ok(text_response(&format!("{B899_TURN2}\nNO_REPLY")));
        }
        // ターン 1（M899_1）と保険: NO_REPLY のみ。
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn audit_899a_no_reply_only_not_delivered_not_saved_not_in_history() {
    let buf = install_capture();
    let mock = Arc::new(NoReplyProbeMock {
        calls: AtomicUsize::new(0),
        turn2_assistant: Mutex::new(None),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    // ターン 1: NO_REPLY のみ。
    let ev1 = "8991".repeat(16);
    fixture.append_line(&mention_event(&ev1, M899_1));

    // ターン 1 の LLM 呼び出しが済むまで待ち、保存の猶予を置く。
    let ran = {
        let mock = mock.clone();
        wait_until(move || mock.calls.load(Ordering::SeqCst) >= 1).await
    };
    assert!(ran, "ターン 1 の LLM 呼び出しが走らない");
    // 決着（apply_delivery_effect → record_agent_no_reply）まで settle する猶予。CI の並列負荷でも
    // 保存が済むよう長めに置く（DB は各テスト独立なので他テストとは干渉しない）。
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 配送 0: NO_REPLY 本文は wire に出ない（沈黙）。BUFFER は binary 全体で共有・累積するため、
    // 「standalone say の本文が NO_REPLY そのもの」に限定して判定する（他テストの本文への substring
    // 誤検出を避ける）。#899 の配送バグはこの NO_REPLY 単体 say を出す。
    assert!(
        !captured(&buf)
            .iter()
            .any(|c| c.kind == "standalone" && c.body.trim() == "NO_REPLY"),
        "NO_REPLY が say として配送された: {:?}",
        captured(&buf)
    );

    // 保存 0: "NO_REPLY" の speech 行が残らない（現 tip は record_agent_no_reply で残る → 赤）。
    let saved = agent_speech_contents(&core, &session_id);
    assert!(
        !saved
            .iter()
            .any(|s| s == "NO_REPLY" || s.contains("NO_REPLY")),
        "NO_REPLY のみの応答が speech に保存された（#899）: {saved:?}"
    );

    // ターン 2: 履歴に NO_REPLY が載っていないことを確かめる。
    let ev2 = "8992".repeat(16);
    fixture.append_line(&mention_event(&ev2, M899_2));
    let turn2_done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B899_TURN2).is_some()).await
    };
    assert!(turn2_done, "ターン 2 の配送が出ない: {:?}", captured(&buf));

    let asst = mock
        .turn2_assistant
        .lock()
        .unwrap()
        .clone()
        .expect("ターン 2 が履歴を捕まえていない");
    assert!(
        !asst
            .iter()
            .any(|m| m.trim() == "NO_REPLY" || m.contains("NO_REPLY")),
        "次ターンの typed 履歴に assistant NO_REPLY が載っている（#899）: {asst:?}"
    );
}

// ---------------------------------------------------------------------------
// #899(b)【§13 #12（本文＋末尾 NO_REPLY→本文 1/保存 1・以降破棄）】: 本文 + NO_REPLY → 本文だけ配送 1・保存 1（NO_REPLY 混入なし）。
// 非回帰の下限固定（現 tip でも緑のはず・NO_REPLY 剥がしが配送/保存両方に効く証拠）。
// ---------------------------------------------------------------------------
const B899B_KEEP: &str = "nr899b-keep 本文はここまで";

#[tokio::test]
async fn audit_899b_body_plus_no_reply_delivers_body_only_saved_once() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text(&format!("{B899B_KEEP} NO_REPLY 破棄されるべき後段"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "899b".repeat(16);
    fixture.append_line(&mention_event(&ev, "NR899B-MARK 本文の後に沈黙"));

    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B899B_KEEP).is_some()).await
    };
    assert!(done, "本文の配送が出ない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 配送 1・NO_REPLY 混入なし。
    let says: Vec<_> = captured(&buf)
        .into_iter()
        .filter(|c| c.body.contains(B899B_KEEP))
        .collect();
    assert_eq!(says.len(), 1, "本文の配送回数が 1 でない: {says:?}");
    assert!(
        says.iter().all(|c| !c.body.contains("NO_REPLY")),
        "配送本文に NO_REPLY が混入: {says:?}"
    );

    // 保存 1・NO_REPLY 混入なし。
    let saved = agent_speech_contents(&core, &session_id);
    let kept: Vec<_> = saved.iter().filter(|s| s.contains(B899B_KEEP)).collect();
    assert_eq!(kept.len(), 1, "本文の保存件数が 1 でない: {saved:?}");
    assert!(
        !saved.iter().any(|s| s.contains("NO_REPLY")),
        "speech に NO_REPLY が保存された: {saved:?}"
    );
}

