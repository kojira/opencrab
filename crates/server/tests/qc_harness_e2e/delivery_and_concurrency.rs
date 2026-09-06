// ==================== (a) mention → say（standalone post） ====================

#[tokio::test]
async fn scenario_a_mention_becomes_say() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text("QCA-ACK 了解、やっておくね");
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let event_id = "a1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "QCA-MARK メンション本文"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.body.contains("QCA-ACK") && c.kind == "standalone")
        })
        .await
    };
    assert!(
        ok,
        "mention の say が dry-run に出ない: {:?}",
        captured(&buf)
    );

    // 1 メンション = 1 ターン。
    assert_eq!(mock.system_prompts().len(), 1, "ターンが 1 本でない");
}

// ============ (A1/A1L) NO_REPLY 終端化 + 破棄ログ（第一柱・DESIGN-RESUME-SETTLE §3.1/§3.1.1）============

/// mock 応答に `…本文… NO_REPLY …ゴミ…` を混入させたとき:
/// (i) 配送 say の body に `NO_REPLY` もゴミも含まれず、前段本文だけで確定する（A1）
/// (ii) 破棄ログ `no_reply_trailing_discarded` が 1 件出て破棄全文と session_id を持つ（A1L）
/// (iii) 破棄テキストは wire（dry-run say）に一切現れない（§3.1.1(c)）
#[tokio::test]
async fn scenario_no_reply_terminates_and_logs_discard() {
    // 互いに部分文字列にならない一意マーカー（グローバルバッファの他テスト混線を避ける）。
    const KEEP: &str = "NRTERM-KEEP 本文はここまで";
    const GARBAGE: &str = "NRTERM-GARBAGE 破棄されるゴミ";

    let buf = install_capture();
    let dbuf = discard_buffer();
    let mock = Arc::new(FifoMock::new());
    mock.push_text(&format!("{KEEP} NO_REPLY {GARBAGE}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let event_id = "d1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "NRTERM-MARK メンション本文"));

    // (i) 前段本文で say が確定するまで待つ。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.body.contains("NRTERM-KEEP") && c.kind == "standalone")
        })
        .await
    };
    assert!(ok, "前段本文の say が出ない: {:?}", captured(&buf));

    // (i) 該当 say の body に NO_REPLY もゴミも含まれない。
    let says: Vec<_> = captured(&buf)
        .into_iter()
        .filter(|c| c.body.contains("NRTERM-KEEP"))
        .collect();
    for s in &says {
        assert!(
            !s.body.contains("NO_REPLY"),
            "say body に NO_REPLY が混入: {:?}",
            s
        );
        assert!(
            !s.body.contains("NRTERM-GARBAGE"),
            "say body に破棄テキストが混入: {:?}",
            s
        );
    }

    // (iii) 破棄テキストはどの dry-run say にも現れない（wire 非搭載）。
    assert!(
        captured(&buf)
            .iter()
            .all(|c| !c.body.contains("NRTERM-GARBAGE")),
        "破棄テキストが wire(dry-run say) に現れた: {:?}",
        captured(&buf)
    );

    // (ii) 破棄ログが 1 件出ており、破棄全文と session_id を持つ（A1L）。
    let ok_discard = {
        let dbuf = dbuf.clone();
        wait_until(move || {
            discards(&dbuf)
                .iter()
                .any(|d| d.discarded.contains("NRTERM-GARBAGE"))
        })
        .await
    };
    assert!(
        ok_discard,
        "破棄ログ(no_reply_trailing_discarded) が出ていない: {:?}",
        discards(&dbuf)
    );
    let d = discards(&dbuf)
        .into_iter()
        .find(|d| d.discarded.contains("NRTERM-GARBAGE"))
        .unwrap();
    assert!(
        d.discarded.contains("NO_REPLY"),
        "破棄全文に NO_REPLY トークンが含まれない: {:?}",
        d
    );
    assert!(
        !d.session_id.is_empty(),
        "破棄ログに session_id 相関キーが無い: {:?}",
        d
    );
}

// ==================== (c) 同一イベントが両車線 → said は 1 回だけ ====================

#[tokio::test]
async fn scenario_c_same_event_on_both_lanes_says_once() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text("QCC-ACK ひとつだけ返すよ");
    // 万一 2 ターン走ったら 2 本目が消費される。後段で system_prompts 数を見て 1 本を確かめる。
    mock.push_text("QCC-EXTRA 余計な二本目");
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // default(mention) 車線 + watch 車線。fake_watch は全車線へ同じ fixture を流すので、
    // 同一行が両車線に届く。自分宛て #p メンションは watch 車線が default 車線へ譲る
    // （Defect A / QC #10）。ネットで said は 1 回・ターンは 1 本・say は 1 通。
    let watches = serde_json::json!([{
        "id": 7,
        "interval_secs": 3600,
        "filter_json": { "authors": [author_pk()] }
    }]);
    let fixture = Fixture::new();
    let (_client, _address, _session_id) =
        wire_instance(&core, &fixture, nostr_config(Some(watches))).await;

    let event_id = "c1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "QCC-MARK 両車線に届く本文"));

    // say が出るまで待つ。
    let ok = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, "QCC-ACK").is_some()).await
    };
    assert!(ok, "say が出ない: {:?}", captured(&buf));

    // 2 本目が来ないことを確かめるための落ち着き時間（interval=3600s なので flush は無い）。
    tokio::time::sleep(Duration::from_millis(400)).await;

    let n_says = captured(&buf)
        .iter()
        .filter(|c| c.body.contains("QCC-ACK"))
        .count();
    assert_eq!(n_says, 1, "同一イベントで say が複数出た: {n_says}");
    assert_eq!(
        mock.system_prompts().len(),
        1,
        "ターンが 1 本でない（両車線で二重に走った）"
    );
}

// ==================== (main) 長い処理中の第2依頼: 3 say が順に出る ====================

// ルーティング用マーカー（会話へ現れる substring・互いに部分文字列にならないよう分離）。
const M_FIRST: &str = "MARKER-ONE";
const M_SECOND: &str = "MARKER-TWO";
const M_SUBTASK: &str = "MARKER-SUB";
// 応答本文（say として観測する。マーカーとも互いとも部分一致しない）。
const B_ACK: &str = "ackbody-alpha 了解、長い処理を始めたよ";
const B_SECOND: &str = "secondbody-beta 回答は 4 だよ";
const B_COMPLETION: &str = "completionbody-gamma 長い処理おわったよ";
const B_SUBTASK_RESULT: &str = "subresult-delta 内部結果";

/// 内容ルーティング mock。指定ターンだけ `Notify` で待たせる（並行ターンが FIFO を奪い合わない）。
struct RoutedMock {
    released: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl RoutedMock {
    fn new() -> Self {
        Self {
            released: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[async_trait::async_trait]
impl LlmProvider for RoutedMock {
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

        // (E) subtask 決着後の resume ターン → 完了報告 say(3)。
        // 注意: システムプロンプトにはツール解説として "subtask_completed" が常に含まれるため、
        // それでは判定できない。決着で親ログに載る subtask 結果本文（会話に現れる）で判定する。
        if text.contains(B_SUBTASK_RESULT) {
            return Ok(text_response(B_COMPLETION));
        }
        // (B) 親ターン#1 の spawn_subtask 実行後（tool_result 有り）→ 即応 ack say(1)。
        if has_tool_role(&request) {
            return Ok(text_response(B_ACK));
        }
        // (D) 第2依頼のターン → 即応 say(2)。
        if text.contains(M_SECOND) {
            return Ok(text_response(B_SECOND));
        }
        // (A) 親ターン#1 の初回 → spawn_subtask を呼んで背景サブタスクを detach。
        if text.contains(M_FIRST) {
            return Ok(tool_call_response(
                "spawn_subtask",
                serde_json::json!({
                    "task": format!("{M_SUBTASK} 長い処理を実行して"),
                    "timeout_secs": 120,
                }),
            ));
        }
        // (C) 背景サブタスクの sub-run → テストが release するまでブロック（= 長い処理の走行中）。
        if text.contains(M_SUBTASK) {
            loop {
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                let waiter = self.notify.notified();
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                waiter.await;
            }
            return Ok(text_response(B_SUBTASK_RESULT));
        }
        Err(anyhow::anyhow!("RoutedMock: unrouted request: {text}"))
    }
}

#[tokio::test]
async fn scenario_main_second_request_not_blocked_during_long_op() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev1 = "b1".repeat(32);
    let ev2 = "b2".repeat(32);

    // 1) 「長い処理して終わったら教えて」→ 即応 ack say(1) ＋ 背景サブタスク detach。
    fixture.append_line(&mention_event(
        &ev1,
        &format!("{M_FIRST} 長い処理して終わったら教えて"),
    ));

    // say(1) が出る AND 背景サブタスクが走行中（held）になるまで待つ。
    let ack_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_ACK).is_some()).await
    };
    assert!(ack_ready, "ack say(1) が出ない: {:?}", captured(&buf));
    let running = {
        let state = core.state.clone();
        let sid = session_id.clone();
        wait_until(move || state.subtask_registries.has_running(&sid)).await
    };
    assert!(
        running,
        "背景サブタスクが走行中にならない（detach/hold 失敗）"
    );

    // 2) 走行中に第2依頼を投入 → ブロックされず即応 say(2)。
    fixture.append_line(&mention_event(&ev2, &format!("{M_SECOND} 2 足す 2 は?")));
    let second_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_SECOND).is_some()).await
    };
    assert!(
        second_ready,
        "第2依頼が長い処理にブロックされている（say(2) 未達）: {:?}",
        captured(&buf)
    );
    // この時点でサブタスクはまだ走行中（held）のはず。
    assert!(
        core.state.subtask_registries.has_running(&session_id),
        "第2依頼処理中にサブタスクが既に終わっている（hold が効いていない）"
    );

    // 3) サブタスクを解放 → 決着 → resume → 完了報告 say(3)。
    mock.release();
    let completion_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_COMPLETION).is_some()).await
    };
    assert!(
        completion_ready,
        "完了報告 say(3) が出ない: {:?}",
        captured(&buf)
    );

    // 3 say が 1→2→3 の順で並ぶ（朝のバグ=第2依頼ブロックが無いことの再現）。
    let i1 = body_index(&buf, B_ACK).expect("ack");
    let i2 = body_index(&buf, B_SECOND).expect("second");
    let i3 = body_index(&buf, B_COMPLETION).expect("completion");
    assert!(
        i1 < i2 && i2 < i3,
        "say の順序が 1→2→3 でない: ack={i1} second={i2} completion={i3} / {:?}",
        captured(&buf)
    );
}

