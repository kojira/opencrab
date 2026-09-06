// ==================== (#898) CONTINUE 途中イテレーションの発話配送・保存 ====================
//
// DESIGN-TURN-CONTINUATION §11.1: 末尾 CONTINUE の生成は「残りの content を通常どおり配送・
// 保存 → 次イテレーション」。reply を使わない純テキストを 3 分割（1回目 CONTINUE / 2回目
// CONTINUE / 3回目）した場合、途中イテレーション（1回目・2回目）の発話も say として配送され、
// memory_sessions に speech として保存されること。#895 は engine 側の on_response_text 発火を
// モックで固定したが extgate V3 の実配線（apply_delivery_effect は最終 EngineResult.response
// のみ配送）を通しておらず、途中発話が配送も保存もされずに落ちていた（#898 QC 実弾で確認）。

/// マーカー（テスト間で共有される dry-run buffer / DB を絞る）。
const C898_1: &str = "C898-1回目。まず一つ";
const C898_2: &str = "C898-2回目。次いこう";
const C898_3: &str = "C898-3回目。これで最後";

#[tokio::test]
async fn scenario_continue_intermediate_speech_delivered_and_saved() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    // reply を使わない純テキスト 3 分割。末尾 CONTINUE で継続、3 回目は継続せず終了。
    mock.push_text(&format!("{C898_1}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{C898_2}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{C898_3}\u{26a1}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "d8".repeat(32);
    fixture.append_line(&mention_event(&ev, "3回に分けて投稿して reply使わずに"));

    // (i) 3 分割の全 say が standalone post として配送される（途中発話 1回目/2回目も届く）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            let says = captured(&buf);
            let has = |needle: &str| {
                says.iter()
                    .any(|c| c.kind == "standalone" && c.body.contains(needle))
            };
            has(C898_1) && has(C898_2) && has(C898_3)
        })
        .await
    };
    assert!(
        delivered,
        "CONTINUE 途中イテレーションの発話が say 配送されない: {:?}",
        captured(&buf)
    );

    // (ii) 配送された say の本文に CONTINUE マーカーが残らない（§11.6）。
    let c898_says: Vec<CapturedSay> = captured(&buf)
        .into_iter()
        .filter(|c| c.body.contains("C898-"))
        .collect();
    assert!(
        c898_says.iter().all(|c| !c.body.contains("CONTINUE")),
        "say 本文に CONTINUE が残留した: {c898_says:?}"
    );

    // (iii) LLM は 3 回呼ばれる（末尾 CONTINUE が 2 回の追加イテレーションを起こす）。
    assert_eq!(
        mock.system_prompts().len(),
        3,
        "末尾 CONTINUE で LLM が 3 回呼ばれていない"
    );

    // (iv) memory_sessions に 3 件の speech が保存され、いずれにも CONTINUE が残らない。
    let speeches: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech" && l.content.contains("C898-"))
            .map(|l| l.content)
            .collect()
    };
    assert_eq!(
        speeches.len(),
        3,
        "途中イテレーションの発話が memory_sessions に保存されていない: {speeches:?}"
    );
    assert!(
        speeches.iter().all(|s| !s.contains("CONTINUE")),
        "保存された speech に CONTINUE が残留した: {speeches:?}"
    );
}

// ==================== (#898 §13.1 j) 途中配送失敗で継続を止める ====================
//
// DESIGN-TURN-CONTINUATION.md §13.1 j「途中イテレーションの投稿が配送失敗（ゲート error）→
// 既存の発話失敗経路（❌/turn_failed）・継続は止める（失敗を隠して次に進まない）」。
// 観測境界: LLM 呼び出し回数（継続が止まれば 1 回だけ・2/3 回目のイテレーションへ進まない）。

const J898_1: &str = "J898-1回目。まず一つ";
const J898_2: &str = "J898-2回目。次いこう";
const J898_3: &str = "J898-3回目。これで最後";

#[tokio::test]
async fn scenario_continue_intermediate_delivery_failure_stops_continuation() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text(&format!("{J898_1}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{J898_2}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{J898_3}\u{26a1}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // 途中発話（say）の配送をゲート error（disconnect）にする。
    core.extgate
        .probe
        .fail_say_write
        .store(true, Ordering::SeqCst);

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e9".repeat(32);
    fixture.append_line(&mention_event(&ev, "3回に分けて・途中で配送失敗"));

    // 少なくとも 1 回目のイテレーションは走る。
    let started = {
        let mock = mock.clone();
        wait_until(move || !mock.system_prompts().is_empty()).await
    };
    assert!(started, "最初のイテレーションすら走っていない");

    // 追加イテレーションが走らないことを確認する猶予（走ってしまうなら継続を止めていない）。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        mock.system_prompts().len(),
        1,
        "途中配送失敗後も継続してしまった（§13.1 j: 継続を止めていない）"
    );
    let _ = &buf;
}

// ==================== (#898 §13 #8) reply×N＋本文＋末尾 CONTINUE ====================
//
// DESIGN-TURN-CONTINUATION §13 #8「reply×N＋本文＋最終行 CONTINUE → 配送 reply N＋本文 1・
// 保存 N+1・次 進む・残留なし」。#904 で reply（発話クラス）＋末尾 CONTINUE が次イテレーションへ
// 進むようになったが、併記された**本文（content）**は最終応答と同じ経路で配送・保存される必要が
// ある（本 PR の in-loop 途中発話配送）。現 tip は reply は配送されるが本文（say）が配送も保存も
// されない → 赤。
// 観測境界: extgate の dry-run 配送（kind="reply" / kind="standalone"）・memory_sessions speech・LLM 回数。

const S8_R1: &str = "S8-返信その1";
const S8_R2: &str = "S8-返信その2";
const S8_BODY: &str = "S8-本文。まとめて一言";
const S8_FINAL: &str = "S8-最終本文";

fn two_replies_with_content(r1: &str, r2: &str, content: &str) -> ChatResponse {
    let mut resp = tool_calls_response(vec![
        ("reply", serde_json::json!({"event": "e1", "text": r1})),
        ("reply", serde_json::json!({"event": "e1", "text": r2})),
    ]);
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

struct S8Mock {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for S8Mock {
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
        // 1 回目: reply×2 ＋ 本文 ＋ 末尾 CONTINUE（継続）。2 回目以降: 最終本文で終端。
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(two_replies_with_content(
                S8_R1,
                S8_R2,
                &format!("{S8_BODY}\u{26a1}\nCONTINUE"),
            ))
        } else {
            Ok(text_response(&format!("{S8_FINAL}\u{26a1}")))
        }
    }
}

#[tokio::test]
async fn scenario_s13_8_reply_plus_body_plus_continue_delivers_body_and_continues() {
    let buf = install_capture();
    let mock = Arc::new(S8Mock {
        calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "f8".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        "S8MARK reply も使いつつ本文も添えて続けて",
    ));

    // (i) reply×2 と本文 say・最終 say がすべて配送される。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            let says = captured(&buf);
            let has = |kind: &str, needle: &str| {
                says.iter()
                    .any(|c| c.kind == kind && c.body.contains(needle))
            };
            has("reply", S8_R1)
                && has("reply", S8_R2)
                && has("standalone", S8_BODY)
                && has("standalone", S8_FINAL)
        })
        .await
    };
    assert!(
        delivered,
        "reply×2＋本文 say＋最終 say のいずれかが配送されない（#8: 本文 say 未配送）: {:?}",
        captured(&buf)
    );

    // (ii) 本文 say（S8_BODY）はちょうど 1 通（1 イテレーション=1 メッセージ）。
    let body_says = captured(&buf)
        .iter()
        .filter(|c| c.kind == "standalone" && c.body.contains(S8_BODY))
        .count();
    assert_eq!(body_says, 1, "本文 say が 1 通でない: {:?}", captured(&buf));

    // (iii) LLM は 2 回（reply＋本文＋CONTINUE で継続 → 2 回目で終端）。
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2, "LLM が 2 回でない");

    // (iv) 残留 CONTINUE なし（配送本文）。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.body.contains("S8-"))
            .all(|c| !c.body.contains("CONTINUE")),
        "配送本文に CONTINUE 残留: {:?}",
        captured(&buf)
    );

    // (v) memory_sessions に本文（S8_BODY）と最終（S8_FINAL）が speech として保存される。
    let speeches: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech")
            .map(|l| l.content)
            .collect()
    };
    assert!(
        speeches.iter().any(|s| s.contains(S8_BODY)),
        "本文（S8_BODY）が memory_sessions に保存されていない: {speeches:?}"
    );
    assert!(
        speeches.iter().any(|s| s.contains(S8_FINAL)),
        "最終本文（S8_FINAL）が memory_sessions に保存されていない: {speeches:?}"
    );
}

