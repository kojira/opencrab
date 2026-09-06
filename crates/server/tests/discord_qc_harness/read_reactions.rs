use super::support::*;

// ===================================================================
// #930【👀 は LLM にその入力を渡した時点で付ける】
// 発端 A だけでなく、ターン走行中に届いて次イテレーションへ畳み込まれた B も、
// B を含む LLM 呼び出しが起きた時点で B へ 👀 を 1 回付ける（返信の後ではない）。
//
// 仕様: DIRECTION-LOG 116/544・DESIGN-DETAIL-RULINGS:43・issue #930。
//
// 再現（root cause #930）: A は execute_shell(sleep) で背景 subtask を起こしつつ、
// ターンを継続（CONTINUE）してセッションロックを保持したまま回る。走行中に B が届くと
// core は B を session log に記録し、A ターンの継続イテレーション（iterations>1）で
// `poll_new_messages` により B を畳み込む（llm_logs: is_bot_iteration=1 に B が入る）。
// だが現状 👀 は `activity started` の origin にだけ付く（run.rs:284）。畳み込みには
// started が出ないので、B の 👀 は「B 自身の後続ターン」の started まで遅れる＝**返信の後**。
//
// 期待（設計）: 畳み込み時点で core が `activity read`+origin(B) を出し、gateway が
// その時点で B へ 👀 を付ける（1 origin 1 回）。＝ B の 👀 は B への返信より**前**。
//
// 観測境界（§1 標準5点）:
//  (1) 配送: B への返信 say が出る。
//  (2) LLM: いずれかの呼び出しが「新着メッセージ」として B を畳み込んでいる（fold 経路の確証）。
//  (3) 👀 回数: A に 1・B に 1（重複 0）。
//  (4) 👀 順序（**赤の核心**）: B の 👀 の capture index が B への最初の返信 say より前。
//      かつ A の宣言 say の後（B 到着時点ではなく畳み込み時点で付く）。
//  (5) 🏁: B への返信に 🏁 0（A の subtask 進行中＝idle でない）。
//  (6) 外形（第2欠陥）: B への返信 say はちょうど 1 件（現 tip は畳み込み＋独立ターンで 2 件＝赤）。
//  (7) 外形: B（6302）への 🤐 は 0（回帰ガード）。
// 現 tip は (4) が **赤**（👀 が返信の後）。(6) は defect-1 や gateway の per-origin dedup を直しても
// 独立ターンが残る限り赤のまま（外形 pin）。本テストの独立ターンは返信変種
// （reply_on_independent=true）で二重返信を say 件数で捉える。
//
// #930 第2欠陥（二重処理）の LLM 呼び出し回数による代理は companion テスト
// `scenario_930_folded_said_does_not_spawn_independent_turn`（NO_REPLY 変種・633 ch）で補助的に持つ。
// ===================================================================
#[tokio::test]
async fn scenario_930_eyes_on_read_folded_midturn_message() {
    let buf = install_capture();
    let mock = Arc::new(EyesOnReadMock {
        reqs: Mutex::new(Vec::new()),
        emitted_sleep: std::sync::atomic::AtomicBool::new(false),
        continues: std::sync::atomic::AtomicUsize::new(0),
        reply_on_independent: true,
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    // sleep を実走させる（背景 subtask）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance_on_channel(&core, &fixture, R930_CH).await;

    // 発端 A: 「sleep して終わったら教えて」。
    fixture.append_message_ch(
        "6301",
        R930_CH,
        &format!("{R930_A} sleep して終わったら教えて"),
    );

    // A の宣言 say が出る＝subtask 起動＆ターンが CONTINUE ループでロック保持中。
    let a_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_DECL))
        })
        .await
    };
    assert!(
        a_ready,
        "A の宣言 say が出ない（subtask/継続未達）: {:?}",
        captured(&buf)
    );
    // 前提: この時点で subtask（sleep 8）が走行中（🏁 抑制条件・false-red 防止）。
    assert!(
        core.state
            .subtask_registries
            .has_running_for_agent(AGENT_ID),
        "A 宣言時点で subtask が走行中でない（sleep 窓を過ぎた・テスト前提崩れ）"
    );

    // 走行中に B が届く（A の宣言配送後・畳み込み前）。
    fixture.append_message_ch(
        "6302",
        R930_CH,
        &format!("{R930_B} その間に date の結果教えて"),
    );

    // B への返信 say が出るまで待つ（sleep 8 の窓内）。
    let breplied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_BREPLY))
        })
        .await
    };
    assert!(breplied, "B への返信 say が出ない: {:?}", captured(&buf));
    // 返信時点でも subtask 走行中（🏁 抑制の前提を明示・false-red 防止）。
    assert!(
        core.state
            .subtask_registries
            .has_running_for_agent(AGENT_ID),
        "B 返信時点で subtask が走行中でない（sleep 窓を過ぎた・テスト前提崩れ）: {:?}",
        captured(&buf)
    );
    // 現 tip は 👀 が返信の後に付く。その遅延 👀 が現れるまでの猶予（付かない/遅い両方を捉える）。
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let caps = captured(&buf);

    // ---- (2) LLM: B が「新着メッセージ」として畳み込まれた呼び出しがある（fold 経路の確証）。----
    let folded = mock
        .reqs
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.contains(R930_B) && t.contains("新着メッセージ"));
    assert!(
        folded,
        "B が走行中ターンへ畳み込まれていない（poll_new_messages 経路を通っていない・テスト前提崩れ）: reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    // ---- (1) 配送: B への返信 say が最低 1 件。----
    let first_breply_idx = caps
        .iter()
        .position(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_BREPLY))
        .expect("B への返信 say の index");

    // ---- (3) 👀 回数: A に 1・B に 1。----
    let eyes_on = |mid: &str| -> Vec<usize> {
        caps.iter()
            .enumerate()
            .filter(|(_, c)| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == mid
            })
            .map(|(i, _)| i)
            .collect()
    };
    let eyes_a = eyes_on("6301");
    let eyes_b = eyes_on("6302");
    assert_eq!(
        eyes_a.len(),
        1,
        "👀 が発端 A（6301）に 1 件でない: {:?}",
        caps
    );
    assert_eq!(
        eyes_b.len(),
        1,
        "👀 が畳み込まれた B（6302）に 1 件でない（0=付かない / 2=重複）: {:?}",
        caps
    );
    let eyes_b_idx = eyes_b[0];

    // A の宣言 say の最初の index（👀 on B が「B 到着時点」でなく「畳み込み時点以降」に付くことの下限）。
    let first_decl_idx = caps
        .iter()
        .position(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_DECL))
        .expect("A の宣言 say の index");

    // ---- (4) 👀 順序（赤の核心）: B の 👀 は B への最初の返信より **前**、かつ宣言の後。----
    assert!(
        eyes_b_idx > first_decl_idx,
        "👀 on B が宣言より前に付いた（畳み込み前に付与）: decl_idx={first_decl_idx} eyes_b_idx={eyes_b_idx} caps={caps:?}"
    );
    assert!(
        eyes_b_idx < first_breply_idx,
        "👀 on B が B への返信より後に付いた（#930: LLM に渡した時点で付けるべき・現 tip は返信後）: eyes_b_idx={eyes_b_idx} first_breply_idx={first_breply_idx} caps={caps:?}"
    );

    // ---- (5) 🏁: B への返信 say に 🏁 0（A の subtask 進行中＝idle でない）。----
    let breply_mids: Vec<String> = caps
        .iter()
        .filter(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_BREPLY))
        .map(|c| c.message.clone())
        .collect();
    let flag_on_breply: usize = breply_mids
        .iter()
        .map(|mid| {
            caps.iter()
                .filter(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && &c.message == mid
                })
                .count()
        })
        .sum();
    assert_eq!(
        flag_on_breply, 0,
        "🏁 が B への返信に付いた（subtask 進行中は idle でない・🏁 0 のはず）: {:?}",
        caps
    );

    // ---- (6) 外形 pin（第2欠陥・二重処理）: B への返信 say はちょうど 1 件（fold の 1 本のみ）。----
    // 現 tip は畳み込みと独立ターンで B に二重返信し 2 件 → 赤。defect-1（👀 タイミング）だけを
    // 直しても、また gateway に per-origin dedup（1 origin 1 回）だけ入れて 👀 系 assert を緑にしても、
    // 独立ターンが残る限りこの外形は赤のまま。畳み込んだ said を消費して独立ターンを起こさない
    // 修正で初めて 1 件になる。
    let breply_says = caps
        .iter()
        .filter(|c| c.kind == "say" && c.channel == R930_CH && c.body.contains(R930_BREPLY))
        .count();
    assert_eq!(
        breply_says, 1,
        "B への返信 say が 1 件でない（畳み込みと独立ターンの二重返信・#930 第2欠陥）: {:?}",
        caps
    );

    // ---- (7) 外形 pin: B（6302）への 🤐 は 0（独立ターンが沈黙終了サインを残さない）。----
    // 実機の独立ターンは NO_REPLY 変種。畳み込んだ said を独立ターンにしない修正後も、B の発端へ
    // 🤐 が付いてはならない（B は畳み込みで読まれ・返信されている）。誤って独立 NO_REPLY ターンを
    // 残すと 🤐 が付き得るため回帰ガードとして pin する。
    let mute_on_b = caps
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "6302")
        .count();
    assert_eq!(
        mute_on_b, 0,
        "🤐 が B（6302）に付いた（畳み込み済みの B へ独立 NO_REPLY ターンの沈黙サイン）: {:?}",
        caps
    );
}

// ===================================================================
// #930 否定側: 走行中に B が届かないターンでは read（👀 の追加付与）は出ない。
// CONTINUE ループ（iterations>1・poll_new_messages が毎回引かれる）でも、新着が無ければ
// 👀 は発端 A の 1 件のみ。＝ read が空 poll で誤発火しないことのガード（恒真防止）。
// ===================================================================
const R930N_CH: &str = "631";
const R930N_A: &str = "R930NAMARK";
const R930N_SAY: &str = "r930n-作業して終わり";

struct NoMidturnMock {
    continues: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for NoMidturnMock {
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
        // 数イテレーション CONTINUE で回してから自然終了（新着注入は無し）。
        let c = self
            .continues
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if c < 3 {
            Ok(text_response(&format!("{R930N_SAY}\nCONTINUE")))
        } else {
            Ok(text_response(R930N_SAY))
        }
    }
}

#[tokio::test]
async fn scenario_930_no_read_reaction_without_midturn_message() {
    let buf = install_capture();
    let mock = Arc::new(NoMidturnMock {
        continues: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance_on_channel(&core, &fixture, R930N_CH).await;

    fixture.append_message_ch("6311", R930N_CH, &format!("{R930N_A} 少し作業して"));

    // ターンの say が出るまで待つ。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930N_CH && c.body.contains(R930N_SAY))
        })
        .await
    };
    assert!(done, "作業 say が出ない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(600)).await;

    let caps = captured(&buf);
    // 👀 は発端 A（6311）に 1 件のみ。他 id への 👀（read 誤発火）は 0。
    let eyes_on_a = caps
        .iter()
        .filter(|c| {
            c.kind == "system_reaction"
                && c.emoji.contains(SYS_ACCEPTED)
                && c.channel == R930N_CH
                && c.message == "6311"
        })
        .count();
    assert_eq!(eyes_on_a, 1, "👀 が発端 A（6311）に 1 件でない: {:?}", caps);
    let eyes_other = caps
        .iter()
        .filter(|c| {
            c.kind == "system_reaction"
                && c.emoji.contains(SYS_ACCEPTED)
                && c.channel == R930N_CH
                && c.message != "6311"
        })
        .count();
    assert_eq!(
        eyes_other, 0,
        "新着の無いターンで 👀 が発端以外へ付いた（read の空 poll 誤発火）: {:?}",
        caps
    );
}

// ===================================================================
// #930 第2欠陥【畳み込んだ said は独立ターンを起こさない（二重処理しない）】
// QC llm_logs（07:45–07:46Z）: 走行中に届いた B は 07:45:39 のイテレーションへ畳み込まれて
// 読まれ（execute_shell(date) 起動）、07:45:55 の resume で返信されたのに、07:46:00 に **B 単独の
// ターンがもう 1 本走って NO_REPLY**。この独立ターンの `activity started`+origin(B) が返信の後に
// 👀 を付ける源だった。＝ B は「畳み込み」と「独立ターン」で二重処理されている。
//
// 期待（設計・補足）: 畳み込んだ said を turn_queues から消費済みにする（既存の said→enqueue_turn
// 経路で「既に文脈に入った seq」を skip する 1 か所。新経路なし）。＝ B を独立ターンとして走らせない。
//
// 観測境界: LLM 呼び出し回数（§1 標準3）。B を「新着メッセージ（畳み込み）」以外の文脈で読む
// 呼び出し＝B が自分の独立ターンを起こした証拠。畳み込み分（新着）のみが正で、それ以外は 0。
// 現 tip は独立ターン（NO_REPLY）が 1 本走る → **赤**。
// ===================================================================
const R930D_CH: &str = "633";

#[tokio::test]
async fn scenario_930_folded_said_does_not_spawn_independent_turn() {
    let buf = install_capture();
    let mock = Arc::new(EyesOnReadMock {
        reqs: Mutex::new(Vec::new()),
        emitted_sleep: std::sync::atomic::AtomicBool::new(false),
        continues: std::sync::atomic::AtomicUsize::new(0),
        reply_on_independent: false,
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance_on_channel(&core, &fixture, R930D_CH).await;

    fixture.append_message_ch(
        "6331",
        R930D_CH,
        &format!("{R930_A} sleep して終わったら教えて"),
    );

    let a_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930D_CH && c.body.contains(R930_DECL))
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

    fixture.append_message_ch(
        "6332",
        R930D_CH,
        &format!("{R930_B} その間に date の結果教えて"),
    );

    // 畳み込み(fold)で B へ返信 say が出るまで待つ。
    let breplied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == R930D_CH && c.body.contains(R930_BREPLY))
        })
        .await
    };
    assert!(
        breplied,
        "fold での B への返信 say が出ない: {:?}",
        captured(&buf)
    );

    // (前提) 畳み込みが実際に起きた（新着メッセージで B を読む呼び出しがある）。
    let folded = mock
        .reqs
        .lock()
        .unwrap()
        .iter()
        .any(|t| t.contains(R930_B) && t.contains("新着メッセージ"));
    assert!(
        folded,
        "B が畳み込まれていない（poll_new_messages 経路未通過・テスト前提崩れ）: reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    // B の独立ターン（畳み込みと二重）の LLM 呼び出しが現れるか、上限まで待つ。
    // 現 tip は現れる（NO_REPLY で沈黙終了する独立ターン）。修正後は現れない（timeout）。
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

    // ---- (赤の核心) 畳み込んだ B を「新着」以外で読む LLM 呼び出し＝独立ターン。0 のはず。----
    let independent_b_calls = mock
        .reqs
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.contains(R930_B) && !t.contains("新着メッセージ"))
        .count();
    assert_eq!(
        independent_b_calls,
        0,
        "畳み込んだ B が独立ターンでも処理された（#930 第2欠陥・二重処理／遅延 👀 の源）: \
         畳み込み以外で B を読む LLM 呼び出しが {independent_b_calls} 件。reqs={:?}",
        mock.reqs.lock().unwrap()
    );

    // (補助) B への返信 say はちょうど 1 件（fold の 1 本のみ・独立ターンは投稿しない）。
    let breply_says = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == R930D_CH && c.body.contains(R930_BREPLY))
        .count();
    assert_eq!(
        breply_says,
        1,
        "B への返信 say が 1 件でない（fold の 1 本のみが正）: {:?}",
        captured(&buf)
    );

    // (補助) 👀 は B に 1 件（畳み込み read 1 回・独立 started の重複 0）。
    let eyes_b = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == "6332"
        })
        .count();
    assert_eq!(
        eyes_b,
        1,
        "👀 が B（6332）に 1 件でない: {:?}",
        captured(&buf)
    );
}
