use super::support::*;

// ===================================================================
// #930【👀 のタイミング・spawned ack 再呼び出し経路（CONTINUE 不使用）】
// QC 実機（llm_logs 07:45Z）では A ターンは execute_shell(sleep) の **spawned ack 後の
// 再呼び出し（is_bot_iteration=1）** で B を畳み込む（CONTINUE ではない）。この経路でも
// 現 tip は 👀 が返信の後に付く（＝#930）。companion の CONTINUE 版と別に、実機どおりの
// tool-call 再呼び出し経路で赤を pin する。
//
// 構成: A→execute_shell(sleep 8)（長い subtask・🏁 抑制）。以降の spawned ack 再呼び出し
// （has_tool_role）では keep-alive に execute_shell(echo) を spawn して turn を継続させる
// （CONTINUE は使わない）。走行中に B を投入 → spawned ack 再呼び出しの poll_new_messages で
// 畳み込み（新着メッセージ）→ 返信。
//
// 観測境界: (1) 畳み込みが spawned ack 再呼び出し（tool 文脈あり・新着メッセージ）で起きる
//  (2) 👀 on B は 1 件・B への返信より前（**赤の核心**）  (3) 🏁 on B 返信 0（subtask 進行中）。
// 現 tip は (2) が赤（👀 が返信の後）。
// ===================================================================
const R930S_CH: &str = "634";
const R930S_A: &str = "R930SAMARK";
const R930S_B: &str = "R930SBMARK";
const R930S_BREPLY: &str = "r930sbreply-date の結果はこう";

struct SpawnedAckFoldMock {
    reqs: Mutex<Vec<String>>,
    step: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SpawnedAckFoldMock {
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
        self.reqs.lock().unwrap().push(text.clone());
        if text.contains(R930S_B) {
            // spawned ack 再呼び出しで「新着メッセージ」として畳み込まれた B → 返信。
            if text.contains("新着メッセージ") {
                return Ok(text_response(R930S_BREPLY));
            }
            // 独立ターンは沈黙（#930 第2欠陥・二重処理）。
            return Ok(text_response("NO_REPLY"));
        }
        let n = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 初回: sleep 8 の背景 subtask（🏁 抑制条件＝agent に running subtask）。
        if n == 0 {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "sleep", "args": ["8"] }),
            ));
        }
        // spawned ack 再呼び出し（has_tool_role）: CONTINUE を使わず echo の spawn で turn を継続
        // し、毎回 poll_new_messages を引かせて B の到着窓を作る（上限つき・暴走防止）。
        if has_tool_role(&request) && n < 25 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [format!("keep-{n}")] }),
            ));
        }
        Ok(text_response(FILLER))
    }
}

#[tokio::test]
async fn scenario_930_eyes_on_read_via_spawned_ack_reinvocation() {
    let buf = install_capture();
    let mock = Arc::new(SpawnedAckFoldMock {
        reqs: Mutex::new(Vec::new()),
        step: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance_on_channel(&core, &fixture, R930S_CH).await;

    fixture.append_message_ch(
        "6341",
        R930S_CH,
        &format!("{R930S_A} sleep して終わったら教えて"),
    );

    // subtask（sleep 8）が走行するまで待つ（registries は Arc 共有）。
    let regs = core.state.subtask_registries.clone();
    let running = {
        let regs = regs.clone();
        wait_until(move || regs.has_running_for_agent(AGENT_ID)).await
    };
    assert!(
        running,
        "A の execute_shell(sleep) subtask が走行しない: {:?}",
        captured(&buf)
    );

    // 走行中（spawned ack 再呼び出しループ中）に B を投入。
    fixture.append_message_ch(
        "6342",
        R930S_CH,
        &format!("{R930S_B} その間に date の結果教えて"),
    );

    let breplied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930S_CH && c.body.contains(R930S_BREPLY))
        })
        .await
    };
    assert!(
        breplied,
        "spawned ack 経路での B への返信 say が出ない: {:?}",
        captured(&buf)
    );
    assert!(
        regs.has_running_for_agent(AGENT_ID),
        "B 返信時点で subtask が走行中でない（sleep 窓を過ぎた・テスト前提崩れ）: {:?}",
        captured(&buf)
    );
    // 現 tip の遅延 👀（独立ターンの started 由来）が現れるまでの猶予。
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // ---- (1) 畳み込みが spawned ack 再呼び出し（tool 文脈＋新着メッセージ）で起きた。----
    let folded_via_spawned_ack = mock.reqs.lock().unwrap().iter().any(|t| {
        t.contains(R930S_B)
            && t.contains("新着メッセージ")
            && (t.contains("spawned") || t.contains("subtask_completed"))
    });
    assert!(
        folded_via_spawned_ack,
        "B が spawned ack 再呼び出し経路で畳み込まれていない（テスト前提崩れ）: reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    let caps = captured(&buf);
    let first_breply_idx = caps
        .iter()
        .position(|c| c.kind == "say" && c.channel == R930S_CH && c.body.contains(R930S_BREPLY))
        .expect("B への返信 say の index");
    let eyes_b: Vec<usize> = caps
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == "6342"
        })
        .map(|(i, _)| i)
        .collect();
    // 👀 on A（発端）も 1 件。
    let eyes_a = caps
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == "6341"
        })
        .count();
    assert_eq!(eyes_a, 1, "👀 が発端 A（6341）に 1 件でない: {:?}", caps);

    // ---- (2) 👀 on B は 1 件・B への返信より前（赤の核心）。----
    assert_eq!(
        eyes_b.len(),
        1,
        "👀 が畳み込まれた B（6342）に 1 件でない（0=付かない / 2=重複）: {:?}",
        caps
    );
    assert!(
        eyes_b[0] < first_breply_idx,
        "👀 on B が B への返信より後に付いた（spawned ack 経路・#930: LLM に渡した時点で付けるべき）: \
         eyes_b_idx={} first_breply_idx={first_breply_idx} caps={caps:?}",
        eyes_b[0]
    );

    // ---- (3) 🏁 on B 返信 0（A の subtask 進行中）。----
    let flag_on_breply: usize = caps
        .iter()
        .filter(|c| c.kind == "say" && c.channel == R930S_CH && c.body.contains(R930S_BREPLY))
        .map(|c| c.message.clone())
        .map(|mid| {
            caps.iter()
                .filter(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                })
                .count()
        })
        .sum();
    assert_eq!(
        flag_on_breply, 0,
        "🏁 が B への返信に付いた（subtask 進行中は 🏁 0）: {:?}",
        caps
    );
}

// ===================================================================
// #933 回帰ピン【1 イテレーションで複数 said を畳み込む → 独立ターン 0】
// 同文の said を 2 件、走行中ターンへ同時に畳み込む。両方 read（👀 各 1）され、畳み込みターンだけ
// が返信し、**どちらも独立ターンを起こさない**（independent_b_calls==0）。#930 の consume-once
// 集合は複数同時畳み込みで 2 件目を取りこぼし得た（#933）。seq 高水位（非消費・単調）へ置換した
// ので 34,35 とも「seq <= 高水位」で skip。deterministic な resume 競合の再現は未達（PR 参照）だが、
// 本ピンは複数同時畳み込みの skip 経路を最外層で固定する回帰ガード。
// ===================================================================
const R933_CH: &str = "641";

#[tokio::test]
async fn scenario_933_multi_said_fold_no_independent_turn() {
    let buf = install_capture();
    let mock = Arc::new(EyesOnReadMock {
        reqs: Mutex::new(Vec::new()),
        emitted_sleep: std::sync::atomic::AtomicBool::new(false),
        continues: std::sync::atomic::AtomicUsize::new(0),
        reply_on_independent: false, // 独立ターンは NO_REPLY（実機の変種）
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance_on_channel(&core, &fixture, R933_CH).await;

    fixture.append_message_ch(
        "6410",
        R933_CH,
        &format!("{R930_A} sleep して終わったら教えて"),
    );
    let a_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R933_CH && c.body.contains(R930_DECL))
        })
        .await
    };
    assert!(
        a_ready,
        "A の宣言 say が出ない（subtask/継続未達）: {:?}",
        captured(&buf)
    );
    assert!(
        core.state
            .subtask_registries
            .has_running_for_agent(AGENT_ID),
        "A 宣言時点で subtask が走行中でない（テスト前提崩れ）"
    );

    // 同文の said を 2 件連続投入（同一 poll 窓に入れ 1 イテレーションで畳み込む）。
    fixture.append_message_ch(
        "6411",
        R933_CH,
        &format!("{R930_B} もういっかい date 教えて"),
    );
    fixture.append_message_ch(
        "6412",
        R933_CH,
        &format!("{R930_B} もういっかい date 教えて"),
    );

    // 畳み込みターンの返信が出るまで待つ。
    let breplied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R933_CH && c.body.contains(R930_BREPLY))
        })
        .await
    };
    assert!(
        breplied,
        "畳み込みターンの返信 say が出ない: {:?}",
        captured(&buf)
    );
    // 独立ターン（畳み込み以外で B を読む LLM 呼び出し）が現れるか、上限まで待つ（現れなければ 0）。
    let _ = {
        let mock = mock.clone();
        wait_until(move || {
            mock.reqs
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.contains(R930_B) && !t.contains("新着メッセージ"))
        })
        .await
    };
    tokio::time::sleep(Duration::from_millis(800)).await;

    let caps = captured(&buf);

    // (前提) 1 イテレーションで 2 件を畳み込んだ（新着メッセージが 1 呼び出しに 2 件）。
    let folded_two = mock
        .reqs
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.matches("新着メッセージ").count() >= 2);
    assert!(
        folded_two,
        "2 said を 1 イテレーションで畳み込めていない（テスト前提崩れ）: reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    // (赤の核心) どちらの said も独立ターンを起こさない。畳み込み以外で B を読む LLM 呼び出し 0。
    let independent_b_calls = mock
        .reqs
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.contains(R930_B) && !t.contains("新着メッセージ"))
        .count();
    assert_eq!(
        independent_b_calls, 0,
        "複数 said 同時畳み込みで独立ターンが走った（#933 skip 漏れ）: 畳み込み以外で B を読む呼び出しが {independent_b_calls} 件。reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    // 👀 は 2 said とも 1 件ずつ（read・重複 0）。
    let eyes_on = |mid: &str| -> usize {
        caps.iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == mid
            })
            .count()
    };
    assert_eq!(
        eyes_on("6411"),
        1,
        "👀 が said1(6411) に 1 件でない: {:?}",
        caps
    );
    assert_eq!(
        eyes_on("6412"),
        1,
        "👀 が said2(6412) に 1 件でない: {:?}",
        caps
    );

    // 返信 say は畳み込みターンの分のみ（独立ターンは NO_REPLY で投稿なし）。
    let breply_says = caps
        .iter()
        .filter(|c| c.kind == "say" && c.channel == R933_CH && c.body.contains(R930_BREPLY))
        .count();
    assert_eq!(
        breply_says, 1,
        "畳み込みターンの返信 say は 1 件のみ（独立ターンの二重返信も塞ぐ・#933）: {:?}",
        caps
    );
}
