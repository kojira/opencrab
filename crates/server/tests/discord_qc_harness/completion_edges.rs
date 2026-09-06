use super::support::*;

// ---------------------------------------------------------------------------
// #915 / §13.2・DIRECTION-LOG 446【say→CONTINUE→NO_REPLY で終わるターン】: 最終生成が NO_REPLY
// （投稿なし）なので 🏁 は 0。途中の say（CONTINUE で進んだ投稿）にも付けない。発話があった
// ターンなので 🤐 も 0。ルール: 🏁 は「ツール呼び出しも CONTINUE も含まない最終生成の自分の
// 投稿」にだけ付く。最終生成に投稿が無ければ付けない。
// ---------------------------------------------------------------------------
// 本文中に "NO_REPLY"/"CONTINUE" の部分文字列を含めない（含めるとサニタイザに途中で切られ、
// 継続ではなく本文＋末尾 NO_REPLY（row 12）扱いになる）。

const SCN_SAY: &str = "scncont-途中で続ける本文だよ";

struct SayContinueThenNoReplyMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SayContinueThenNoReplyMock {
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
            // 1 生成目: 本文＋末尾 CONTINUE（途中の投稿・継続）。
            0 => text_response(&format!("{SCN_SAY}\nCONTINUE")),
            // 2 生成目: NO_REPLY（投稿なしで終端）。
            _ => text_response("NO_REPLY"),
        })
    }
}

#[tokio::test]
async fn scenario_915_say_continue_then_no_reply_gets_no_flag() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(SayContinueThenNoReplyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9160", "SCNMARK 続けてから最後は黙る");

    // 途中 say（SCN_SAY）が配送されるまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SCN_SAY))
        })
        .await
    };
    assert!(
        delivered,
        "途中 say（SCN_SAY）が配送されない: {:?}",
        captured(&buf)
    );
    // 最終 NO_REPLY 決着まで猶予（🏁/🤐 の付与はターン終了時）。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SCN_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("SCN_SAY say の message id");

    // 🏁 は途中 say に付かない（最終生成は NO_REPLY＝投稿なし → 🏁 0）。現 tip は say 配送ごとに
    // 付けるため途中 say に 🏁 が付く → 赤。
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        0,
        "🏁 が途中 say に誤付与（最終 NO_REPLY のターンは 🏁 0・DIRECTION-LOG 446）: {:?}",
        captured(&buf)
    );
    // 発端 9160 にも 🏁 は付かない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9160"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が発端に誤付与: {:?}",
        captured(&buf)
    );

    // 発話（途中 say）があったターンなので 🤐 も 0（沈黙終了ではない）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9160")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "🏁 発話ありターンに 🤐 が誤付与（say→CONTINUE→NO_REPLY は 🤐 0）: {:?}",
        captured(&buf)
    );

    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "say→CONTINUE→NO_REPLY の LLM 呼び出しが 2 でない"
    );
}

// ---------------------------------------------------------------------------
// #915 / DIRECTION-LOG 446【reply→CONTINUE→reaction のみで終わるターン】: 最終生成が reaction のみ
// （reaction は「投稿」ではない）なので 🏁 は 0。途中の reply（CONTINUE で進んだ投稿）にも付けない。
// reply/reaction は invoke 経路で say consumer を通らないため現 tip でも 0（回帰ガード）。
// ---------------------------------------------------------------------------
const RR_REPLY: &str = "rrreply-途中の返信（最終は reaction のみ）";
const RR_EMOJI: &str = "✅";

struct ReplyContinueThenReactionMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyContinueThenReactionMock {
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
            // 1 生成目: reply（本文 RR_REPLY）＋末尾 CONTINUE（本文は空＝継続）→ 進む。
            0 => reply_with_content_response(RR_REPLY, "CONTINUE"),
            // 2 生成目: reaction のみ（投稿なしで終端）。
            _ => tool_call_response(
                "reaction",
                serde_json::json!({"event": "e1", "emoji": RR_EMOJI}),
            ),
        })
    }
}

#[tokio::test]
async fn scenario_915_reply_continue_then_reaction_only_gets_no_flag() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReplyContinueThenReactionMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9161", "RRMARK 返信してから最後はリアクションだけ");

    // 最終 reaction（RR_EMOJI・発端 9161）が配送されるまで待つ。
    let reacted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.emoji.contains(RR_EMOJI) && c.message == "9161")
        })
        .await
    };
    assert!(
        reacted,
        "最終 reaction が配送されない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 途中 reply は配送された。
    let reply_delivered = captured(&buf)
        .iter()
        .any(|c| c.kind == "reply" && c.body.contains(RR_REPLY));
    assert!(
        reply_delivered,
        "途中 reply が配送されない: {:?}",
        captured(&buf)
    );

    // 🏁 は 0（最終生成は reaction のみ＝投稿なし）。発端 9161 にも付かない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9161"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が reaction 終端ターンに誤付与（最終生成 reaction のみ → 🏁 0）: {:?}",
        captured(&buf)
    );
    // 発話ありターンなので 🤐 も 0。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9161")
        .count();
    assert_eq!(muted_on_origin, 0, "🤐 が誤付与: {:?}", captured(&buf));

    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "reply→CONTINUE→reaction の LLM 呼び出しが 2 でない"
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 6【reply×N（本文なし・単一生成）→ 最後の reply に 1】: 1 生成で reply を 3 本
// 出すターン。🏁 は最後の reply の own 投稿 id（reply_id）に 1・他 0・総数 1。reply は invoke 経路
// なので現 tip は 🏁 0 → **赤**。相関は capture の `reply_id`（dry-run が合成した own 投稿 id）で行う
// （`message`＝返信先 origin とは分離）。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn scenario_915_reply3_flag_on_last_reply() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9170", &format!("{M_REPLY3} 3 本まとめて返信して"));

    // reply 3 本が配送されるまで待つ（各 reply は distinct な reply_id を持つ）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_REPLY3.iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && !c.reply_id.is_empty())
            })
        })
        .await
    };
    assert!(delivered, "reply×3 が配送されない: {:?}", captured(&buf));
    // 決着（ended）まで猶予。🏁 はターン終了時付与。
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 各 reply の own 投稿 id（reply_id）を本文で引く。
    let reply_id_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "reply" && c.body.contains(body))
            .map(|c| c.reply_id.clone())
            .filter(|m| !m.is_empty())
    };
    let last_reply_id = reply_id_of(B_REPLY3[2]).expect("最後の reply の reply_id");
    let first_reply_id = reply_id_of(B_REPLY3[0]).expect("1 本目 reply の reply_id");
    let second_reply_id = reply_id_of(B_REPLY3[1]).expect("2 本目 reply の reply_id");

    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 🏁 は最後の reply に 1・途中 2 本には 0（§13.3.6 row 6）。現 tip は reply に 🏁 0 → 赤。
    assert_eq!(
        completed_on(&last_reply_id),
        1,
        "🏁 が最後の reply に 1 件で付かない（§13.3.6 row 6・現 tip は reply に 🏁 0）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&first_reply_id),
        0,
        "🏁 が 1 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&second_reply_id),
        0,
        "🏁 が 2 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    let total: usize = [&first_reply_id, &second_reply_id, &last_reply_id]
        .iter()
        .map(|id| completed_on(id))
        .sum();
    assert_eq!(
        total,
        1,
        "reply×3 ターンの 🏁 総数が 1 でない: {:?}",
        captured(&buf)
    );

    // 発端 9170 にも 🏁 は付かない（🏁 は自分の投稿へ）。
    let on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9170"
        })
        .count();
    assert_eq!(on_origin, 0, "🏁 が発端に誤付与: {:?}", captured(&buf));
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 14【reply×N + NO_REPLY（同一生成）→ 最後の reply に 1】: 1 生成で reply を 2 本
// ＋本文 NO_REPLY。reply は配送され、最終応答は NO_REPLY（say なし）。🏁 は最後の reply の reply_id
// に 1・他 0・総数 1。発話ありなので 🤐 0。reply は invoke 経路で現 tip は 🏁 0 → **赤**。
// ---------------------------------------------------------------------------
const RNR_1: &str = "rnr-返信1";
const RNR_2: &str = "rnr-返信2（最後）";

struct Reply2ThenNoReplyMock;

#[async_trait::async_trait]
impl LlmProvider for Reply2ThenNoReplyMock {
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
        let mut resp = tool_calls_response(vec![
            ("reply", serde_json::json!({"event": "e1", "text": RNR_1})),
            ("reply", serde_json::json!({"event": "e1", "text": RNR_2})),
        ]);
        resp.choices[0].message.content = Some(MessageContent::Text("NO_REPLY".to_string()));
        Ok(resp)
    }
}

#[tokio::test]
async fn scenario_915_reply2_then_no_reply_flag_on_last_reply() {
    let buf = install_capture();
    let mock = Arc::new(Reply2ThenNoReplyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9180", "RNRMARK 2 本返信して最後は黙る");

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            [RNR_1, RNR_2].iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && !c.reply_id.is_empty())
            })
        })
        .await
    };
    assert!(delivered, "reply×2 が配送されない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(600)).await;

    let reply_id_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "reply" && c.body.contains(body))
            .map(|c| c.reply_id.clone())
            .filter(|m| !m.is_empty())
    };
    let last_id = reply_id_of(RNR_2).expect("最後の reply の reply_id");
    let first_id = reply_id_of(RNR_1).expect("1 本目 reply の reply_id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&last_id),
        1,
        "🏁 が最後の reply に 1 件で付かない（§13.3.6 row 14・現 tip は reply に 🏁 0）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&first_id),
        0,
        "🏁 が 1 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    // 発話（reply）ありなので発端 9180 に 🤐 は付かない。
    let muted = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9180")
        .count();
    assert_eq!(
        muted,
        0,
        "reply ありターンに 🤐 が誤付与: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 12【本文 + 末尾 NO_REPLY → 最終 say に 1】: 本文は配送され（NO_REPLY 以降は破棄）、
// 最終生成の投稿＝その say。🏁 はその say に 1。現 tip も say に付く（回帰ガード）。
// ---------------------------------------------------------------------------
// 本文中に "NO_REPLY"/"CONTINUE" の部分文字列を含めない（サニタイザに途中で切られないため）。
const BNR_SAY: &str = "bnrsay-末尾マーカーで黙る本文だよ";

struct BodyThenTrailingNoReplyMock;

#[async_trait::async_trait]
impl LlmProvider for BodyThenTrailingNoReplyMock {
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
        Ok(text_response(&format!("{BNR_SAY}\nNO_REPLY")))
    }
}

#[tokio::test]
async fn scenario_915_body_then_trailing_no_reply_flag_on_say() {
    let buf = install_capture();
    let mock = Arc::new(BodyThenTrailingNoReplyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9181", "BNRMARK 本文を出して末尾で黙る");

    let say_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(BNR_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match mid {
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
        say_completed,
        "本文 say＋🏁 が観測できない（本文が配送されないか 🏁 が付かない）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(BNR_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("BNR_SAY say の message id");
    let completed = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed,
        1,
        "🏁 が本文 say に 1 件で付かない（§13.3.6 row 12）: {:?}",
        captured(&buf)
    );
    // 本文に NO_REPLY マーカーが残らない（回帰）。
    assert!(
        !captured(&buf)
            .iter()
            .any(|c| c.kind == "say" && c.body.contains("NO_REPLY")),
        "say 本文に NO_REPLY が残留: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 7【reply×N + 本文（同一生成）→ 到着順で最後の投稿に 1】: 1 生成で reply×2＋本文。
// reply は invoke で先に配送、本文 say は最終応答として後に配送＝到着順で最後は say。🏁 はその say に
// 1・reply には 0。現 tip も say に付く（回帰ガード）。
// ---------------------------------------------------------------------------
const R7_1: &str = "r7-返信1";
const R7_2: &str = "r7-返信2";
const R7_SAY: &str = "r7say-本文（到着順で最後）";

struct Reply2PlusBodyMock;

#[async_trait::async_trait]
impl LlmProvider for Reply2PlusBodyMock {
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
        let mut resp = tool_calls_response(vec![
            ("reply", serde_json::json!({"event": "e1", "text": R7_1})),
            ("reply", serde_json::json!({"event": "e1", "text": R7_2})),
        ]);
        resp.choices[0].message.content = Some(MessageContent::Text(R7_SAY.to_string()));
        Ok(resp)
    }
}

#[tokio::test]
async fn scenario_915_reply2_plus_body_flag_on_last_post_say() {
    let buf = install_capture();
    let mock = Arc::new(Reply2PlusBodyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9182", "R7MARK 2 本返信して本文も出して");

    let say_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R7_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match mid {
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
        say_completed,
        "本文 say＋🏁 が観測できない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R7_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("R7_SAY say の message id");
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        1,
        "🏁 が到着順で最後の投稿（say）に 1 件で付かない（§13.3.6 row 7）: {:?}",
        captured(&buf)
    );
    // reply には 🏁 は付かない（最後の投稿は say）。
    let reply_id_2 = captured(&buf)
        .iter()
        .find(|c| c.kind == "reply" && c.body.contains(R7_2))
        .map(|c| c.reply_id.clone())
        .filter(|m| !m.is_empty());
    if let Some(rid) = reply_id_2 {
        let on_reply = captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == rid
            })
            .count();
        assert_eq!(
            on_reply,
            0,
            "🏁 が reply に誤付与（最後の投稿は say のはず）: {:?}",
            captured(&buf)
        );
    }
}
