use super::support::*;

// =====================================================================================
// 監査ピン #900(a/c)【§13 #6（reply×N 本文なし→reply N 配送/保存 N/🤐 付けない）／ターン合計 reply3-in-one・
// §13.1 g（reaction/repost のみも #6 と同じ）の reply 版】: reply×3-in-one（発話クラスのみのターン）→ 配送 3・LLM 1・🤐 なし。
//
// 現 tip: reply×3 は撃ちっぱなしで配送されるが、最終本文が空（say 0）のためゲートが沈黙と解釈し
// CompletedNoReply → 🤐 を発端へ付ける（#883 発話クラス化の契約列挙漏れ）。→ 🤐 の pin で赤。
// 期待: 発話（say/reply/reaction）が 1 つでもあったターンには 🤐 を付けない。
//
// 既存 scenario_a3_three_replies_...（qc_harness_e2e）は配送 3・LLM 1 を pin 済みだが 🤐 反応は
// 観測していない。ここは discord ゲートの system reaction を観測できる唯一のハーネスなので
// 「足りない観測点＝🤐 なし」だけを追加する。
// =====================================================================================
const M_AUDIT_REPLY3: &str = "AUDITREPLY3MARK";
const B_R3_1: &str = "r3body-one 一通目だよ";
const B_R3_2: &str = "r3body-two 二通目だよ";
const B_R3_3: &str = "r3body-three 三通目だよ";

struct ThreeReplyMock {
    calls: std::sync::atomic::AtomicUsize,
}

fn three_reply_response() -> ChatResponse {
    let tc = |text: &str| ToolCall {
        id: format!("tc-{}", uuid::Uuid::new_v4()),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "reply".to_string(),
            arguments: serde_json::json!({"event": "e1", "text": text}).to_string(),
        },
    };
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(vec![tc(B_R3_1), tc(B_R3_2), tc(B_R3_3)]),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

#[async_trait::async_trait]
impl LlmProvider for ThreeReplyMock {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = request_text(&request);
        if !has_tool_role(&request) && text.contains(M_AUDIT_REPLY3) {
            return Ok(three_reply_response());
        }
        // 撃ちっぱなしなので追加ターンは来ないはずだが、保険で沈黙終端。
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn audit_900c_utterance_only_reply_turn_gets_no_muted_reaction() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ThreeReplyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("900", &format!("{M_AUDIT_REPLY3} 3回に分けて返信して"));

    // reply×3 がすべて配送されるまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            [B_R3_1, B_R3_2, B_R3_3].iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b))
            })
        })
        .await
    };
    assert!(delivered, "reply×3 が配送されない: {:?}", captured(&buf));

    // 🤐 判定は決着時に立つので、決着の猶予を置く。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 配送 3: reply が 3 通。
    for b in [B_R3_1, B_R3_2, B_R3_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(b))
            .count();
        assert_eq!(
            n,
            1,
            "reply {b} の配送回数が 1 でない: {:?}",
            captured(&buf)
        );
    }

    // LLM 1 回（reply×3 は 1 生成に並ぶ・ack ごとの再呼び出しなし）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        1,
        "reply×3 が 1 生成で完了していない（LLM 呼び出しが 1 でない）"
    );

    // 🤐 なし: 発話があったターンなので発端 900 に 🤐 は付かない（現 tip は付く → 赤）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "900")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話（reply×3）があったターンに 🤐 が誤発火（#900: 発話クラスの契約列挙漏れ）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// §13.1 c【Discord で 1 イテレーション = 1 メッセージ（結合/編集しない）】: 本文＋CONTINUE で
// 3 分割 → Discord に 3 メッセージが別々に出る（#898 の discord レーン版）。
// 現 tip: 配送層が最終応答（er.response）だけを say するので最後の 1 メッセージだけ → 赤。
// §13 #2 を 3 連鎖／ターン合計 plain3 の Discord レーン。
// ---------------------------------------------------------------------------
const CC_1: &str = "CCsplit-one 一通目の本文";
const CC_2: &str = "CCsplit-two 二通目の本文";
const CC_3: &str = "CCsplit-three 三通目の本文";

struct ContinueSplitMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ContinueSplitMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            0 => text_response(&format!("{CC_1}\nCONTINUE")),
            1 => text_response(&format!("{CC_2}\nCONTINUE")),
            _ => text_response(CC_3),
        })
    }
}

#[tokio::test]
async fn audit_s13_1c_continue_split_is_separate_discord_messages() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ContinueSplitMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("1301", "CCMARK 3回に分けて投稿して reply使わずに");

    // 最終メッセージ（CC_3）が出るまで待つ（= 3 イテレーションに到達）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(CC_3))
        })
        .await
    };
    assert!(done, "3 通目（最終）が出ない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 各イテレーションが別々の 1 メッセージとして出る（現 tip は CC_3 のみ → 赤）。
    for m in [CC_1, CC_2, CC_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.body.contains(m))
            .count();
        assert_eq!(
            n,
            1,
            "分割 {m} の Discord メッセージが 1 通でない（#898 discord: 途中発話未配送）: {:?}",
            captured(&buf)
        );
    }
    // 結合していない: どの 1 メッセージも 2 マーカーを同時に含まない。
    assert!(
        captured(&buf).iter().filter(|c| c.kind == "say").all(|c| {
            [CC_1, CC_2, CC_3]
                .iter()
                .filter(|m| c.body.contains(*m))
                .count()
                <= 1
        }),
        "分割メッセージが 1 通に結合された（1 イテレーション=1 メッセージに反する）: {:?}",
        captured(&buf)
    );
    // LLM 3・残留 CONTINUE なし。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        3,
        "CONTINUE 3 分割の LLM 呼び出しが 3 でない"
    );
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && [CC_1, CC_2, CC_3].iter().any(|m| c.body.contains(m)))
            .all(|c| !c.body.contains("CONTINUE")),
        "say に CONTINUE が残留: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915【🏁 はターン終了時のみ・途中投稿には付けない】: 純 say の末尾 CONTINUE で 3 分割した
// ターン（本文＋CONTINUE ×2 → 本文）で、🏁（完了サイン）は**最後の say メッセージ 1 件だけ**に
// 付き、途中の 2 件には付かない。オーナー裁定（逐語）:「🏁を付けるのは次のターンがない時だけ
// です」「続きがないことを知らせるものですよ」。
//
// 観測境界（dry-run capture）: kind="say" の各分割メッセージの own message id と、kind=
// "system_reaction"・emoji=🏁 の付け先 message を相関する。現 tip は say 配送ごとに 🏁 を付ける
// ため途中 2 件にも 🏁 が付く → 赤。修正後は activity ended で最後の say だけに付く → 緑。
// LLM 3・say 配送 3・🏁 は最終 1 件のみ・途中 0 件・🤐 0（発話ありターン）。
// ---------------------------------------------------------------------------
const FC_1: &str = "flagcont-one 一通目";
const FC_2: &str = "flagcont-two 二通目";
const FC_3: &str = "flagcont-three 三通目";

struct FlagContinueSplitMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for FlagContinueSplitMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            0 => text_response(&format!("{FC_1}\nCONTINUE")),
            1 => text_response(&format!("{FC_2}\nCONTINUE")),
            _ => text_response(FC_3),
        })
    }
}

#[tokio::test]
async fn scenario_915_completed_flag_only_on_last_say_of_continue_split() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(FlagContinueSplitMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("915", "FCMARK 3回に分けて返信して reply使わずに");

    // 最終メッセージ（FC_3）が say として出るまで待つ（= 3 イテレーションに到達・say 配送 3）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FC_3))
        })
        .await
    };
    assert!(done, "3 通目（最終 say）が出ない: {:?}", captured(&buf));

    // 最終 say（FC_3）の own message id に 🏁 が付くまで待つ（決着＝activity ended で付与）。
    // ヘルパ: 本文で分割 say を特定し own message id を返す（BUFFER は共有なので本文で scope）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let saw_last_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let last_mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FC_3))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match last_mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        saw_last_completed,
        "🏁 が最終 say（FC_3）に付かない: {:?}",
        captured(&buf)
    );
    // 決着後、途中投稿の誤付与が無いことを確定するための猶予（バグ時は配送ごとに即付くので既に出ている）。
    tokio::time::sleep(Duration::from_millis(400)).await;

    let first_mid = mid_of(FC_1).expect("FC_1 say の message id");
    let second_mid = mid_of(FC_2).expect("FC_2 say の message id");
    let third_mid = mid_of(FC_3).expect("FC_3 say の message id");

    let completed_on = |mid: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == mid
            })
            .count()
    };

    // 🏁 は最終 say（FC_3）1 件のみ。途中の 2 件（FC_1/FC_2）には付かない（現 tip は付く → 赤）。
    assert_eq!(
        completed_on(&first_mid),
        0,
        "🏁 が途中 say FC_1 に誤って付いている（ターン終了時のみの裁定違反）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&second_mid),
        0,
        "🏁 が途中 say FC_2 に誤って付いている（ターン終了時のみの裁定違反）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&third_mid),
        1,
        "🏁 が最終 say FC_3 に 1 件付かない: {:?}",
        captured(&buf)
    );

    // ターン全体での 🏁 総数は 1（分割 3 メッセージ合計）。
    let total_completed: usize = [&first_mid, &second_mid, &third_mid]
        .iter()
        .map(|m| completed_on(m))
        .sum();
    assert_eq!(
        total_completed,
        1,
        "分割ターンの 🏁 総数が 1 でない（途中付与のバグ）: {:?}",
        captured(&buf)
    );

    // 発話ありターンなので発端 915 に 🤐 は付かない（回帰）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "915")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話ありターンに 🤐 が誤発火: {:?}",
        captured(&buf)
    );

    // LLM 3・say 配送 3・CONTINUE 残留なし（回帰）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        3,
        "CONTINUE 3 分割の LLM 呼び出しが 3 でない"
    );
    for m in [FC_1, FC_2, FC_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(m))
            .count();
        assert_eq!(n, 1, "分割 {m} の say が 1 通でない: {:?}", captured(&buf));
    }
}

// ---------------------------------------------------------------------------
// #915 / §13.2 表 row 8【reply → CONTINUE → say】: reply を配送してから CONTINUE で継続し、
// 最終イテレーションで say を投稿するターン。🏁 はターン終了時（activity ended）の最後の投稿
// ＝最終 say に **1 件だけ**。途中の reply には付けない（own say id で相関・count で pin）。
// ---------------------------------------------------------------------------
const RC_REPLY: &str = "rcreply-途中の返信本文";
const RC_SAY: &str = "rcsay-最終の通常発言";

struct ReplyThenContinueThenSayMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyThenContinueThenSayMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            // 1 生成目: reply（本文 RC_REPLY）＋末尾 CONTINUE（本文は空＝継続のみ）→ 進む。
            0 => reply_with_content_response(RC_REPLY, "CONTINUE"),
            // 2 生成目: 純 say（最終・CONTINUE なし）→ ターン終了。
            _ => text_response(RC_SAY),
        })
    }
}

#[tokio::test]
async fn scenario_915_reply_then_continue_then_say_flag_only_on_last_say() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReplyThenContinueThenSayMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9153", &format!("{M_REPLY} からの CONTINUE で最後は say"));

    // 最終 say（RC_SAY）の own message id に 🏁 が付くまで待つ（決着＝activity ended で付与）。
    let saw_last_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let last_mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(RC_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match last_mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        saw_last_completed,
        "🏁 が最終 say（RC_SAY）に付かない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(400)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(RC_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("RC_SAY say の message id");

    // 🏁 は最終 say に 1 件のみ（§13.2 row 8）。
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        1,
        "🏁 が最終 say に 1 件で付かない（§13.2 row 8）: {:?}",
        captured(&buf)
    );

    // 途中の reply は配送された（kind="reply"・own reply_id あり）。
    let reply_id = captured(&buf)
        .iter()
        .find(|c| c.kind == "reply" && c.body.contains(RC_REPLY))
        .map(|c| c.reply_id.clone())
        .filter(|m| !m.is_empty())
        .expect("途中 reply の reply_id");
    // §13.3.6 row 9（非ブロック指摘・DIRECTION-LOG 追加）: 途中 reply の reply_id には 🏁 0。
    let completed_on_reply = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == reply_id
        })
        .count();
    assert_eq!(
        completed_on_reply,
        0,
        "🏁 が途中 reply（reply_id）に誤付与（最終イテレーションの投稿のみ・§13.3.6 row 9）: {:?}",
        captured(&buf)
    );
    // 発端 9153（reply 先）にも付けない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9153"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が発端 9153（reply 先）に誤って付いている（§13.3.6）: {:?}",
        captured(&buf)
    );
    // ターン全体で 🏁 は 1（最終 say のみ・途中 reply/発端は 0）。
    let total = completed_on_say + completed_on_reply + completed_on_origin;
    assert_eq!(
        total,
        1,
        "reply→CONTINUE→say の 🏁 総数が 1 でない（§13.3.6 row 9）: {:?}",
        captured(&buf)
    );

    // LLM 2 回（reply+CONTINUE → 最終 say）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "reply→CONTINUE→say の LLM 呼び出しが 2 でない"
    );
}
