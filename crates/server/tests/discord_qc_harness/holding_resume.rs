use super::support::*;

// ---------------------------------------------------------------------------
// #916 §13.4.1【holding 本文（本文＋execute_shell）が V3 で配送・保存され、宣言に 🏁 0・
// resume 報告に 🏁 1・ack 後 NO_REPLY で追加投稿 0/🤐 0】:
//   1 生成目 = content(宣言)＋execute_shell(echo) の holding。現 tip は holding 本文を
//   gateway へ配送せず on_tool_call で保存だけする（skill_engine.rs:1109-1110「holding は
//   従来経路」・:850-856 で content は on_tool_call のみ）→ 宣言 say が dry-run に出ない → 赤。
//   spawn ack（spawned 合成結果＝tool role）は NO_REPLY でターンを閉じる（追加投稿 0）。
//   subtask（echo）決着 → resume 完了報告 say。宣言に 🏁 なし・報告に 🏁 1。
// 観測境界: dry-run kind="say"（配送）・memory_sessions speech（保存）・system_reaction（🏁/🤐）。
// ---------------------------------------------------------------------------

const M_916: &str = "HOLD916MARK";
const H916_DECL: &str = "h916decl-調べるね（holding 宣言・execute_shell と同一生成）";
const H916_REPORT: &str = "h916report-終わったよ（resume 完了報告）";
const H916_ECHO: &str = "h916-echo-即時stdout";

struct HoldingShellMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for HoldingShellMock {
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
        // spawn ターンの ack（合成 "spawned" 結果＝tool role）→ NO_REPLY で追加投稿せず閉じる。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        // 初回（tool role 無し・1 回だけ）→ 本文（宣言）＋execute_shell を同一生成で（holding）。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(shell_with_content_response(H916_DECL, "echo", &[H916_ECHO]));
        }
        // subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 完了報告 say。
        Ok(text_response(H916_REPORT))
    }
}

#[tokio::test]
async fn scenario_916_holding_body_delivered_and_saved() {
    let buf = install_capture();
    let mock = Arc::new(HoldingShellMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9220", &format!("{M_916} sleep 60 して終わったら教えて"));

    // §13.4.1 手順 2/4: 宣言 holding say（H916_DECL）と resume 報告 say（H916_REPORT）が両方出るまで待つ。
    // 現 tip は holding 本文が V3 で未配送のため H916_DECL の say が出ず、この wait は false → 赤。
    let both = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            caps.iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(H916_DECL))
                && caps.iter().any(|c| {
                    c.kind == "say" && c.channel == CHANNEL && c.body.contains(H916_REPORT)
                })
        })
        .await
    };
    assert!(
        both,
        "宣言 holding say（本文＋execute_shell）と resume 報告 say が揃わない（現 tip: holding 本文が V3 で未配送・§13.4.1 手順 2/5）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 配送 count: 宣言 say 1・報告 say 1（§13.4.1 手順 2/4）。
    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .count()
    };
    assert_eq!(
        say_count(H916_DECL),
        1,
        "宣言 holding 本文が Discord に 1 件配送されていない（現 tip 0＝V3 未配送）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(H916_REPORT),
        1,
        "resume 完了報告が 1 件配送されていない: {:?}",
        captured(&buf)
    );

    // 否定側 1: ack の NO_REPLY は配送しない（追加投稿 0・§13.4.1 手順 3）。
    let noreply_say = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains("NO_REPLY"))
        .count();
    assert_eq!(
        noreply_say,
        0,
        "ack の NO_REPLY が投稿された（追加投稿・§13.4.1 手順 3）: {:?}",
        captured(&buf)
    );

    // 否定側 2: 発話ありターンなので発端 9220 に 🤐 は付かない（§13.4.1 手順 3）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9220")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話ありターンに 🤐 が誤発火（§13.4.1 手順 3・追加投稿 0/🤐 0）: {:?}",
        captured(&buf)
    );

    // 🏁: 宣言 say に 0（進行中＝subtask 未決着）・報告 say に 1（§13.4.1 手順 2/4）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let decl_mid = mid_of(H916_DECL).expect("宣言 say の message id");
    let report_mid = mid_of(H916_REPORT).expect("報告 say の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&decl_mid),
        0,
        "🏁 が spawned 宣言 say に誤付与（進行中は付けない・§13.4.1 手順 2）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&report_mid),
        1,
        "🏁 が resume 完了報告 say に 1 件で付かない（§13.4.1 手順 4）: {:?}",
        captured(&buf)
    );

    // 保存＝配送の一致（§13.4.1 手順 5）: memory_sessions の自 speech は宣言＋報告の 2 行のみ。
    // 現 tip は宣言 holding が保存だけされ配送されない（保存 2・配送 1）＝Discord に出ていない文が
    // 保存されている状態。配送（say 2）と保存（2 行）の一致で「出ていない文が保存されていない」を固定。
    let own_speech_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        own_speech_rows, 2,
        "自 speech 保存行が宣言＋報告の 2 行でない（§13.4.1 手順 5）: {own_speech_rows}"
    );
    let delivered_says = say_count(H916_DECL) + say_count(H916_REPORT);
    assert_eq!(
        delivered_says as i64, own_speech_rows,
        "配送数（say {delivered_says}）と保存数（speech {own_speech_rows}）が一致しない（Discord に出ていない文が保存されている・§13.4.1 手順 5）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #918 §13.4.2【resume ターンで 本文＋CONTINUE→本文 → 配送 2・保存 2・🏁 は 2 件目のみ・
// CONTINUE 残留 0】:
//   spawn ターンで宣言（別生成）→ subtask（echo）決着 → resume ターンが本文1＋末尾CONTINUE →
//   本文2 で 2 分割。resume の途中発話（本文1）が配送・保存され、最終（本文2）に 🏁 1、途中に 🏁 0、
//   どの say にも "CONTINUE" が出ない。
//   注（実装調査）: base tip 9dc50f35(#917) が completion.rs:194 に on_continuation_speech を配線済み。
//   本テストは #918 の現状（赤 or 緑）を実証する探り。緑なら「#917 で解消済み」を確定させる回帰。
// ---------------------------------------------------------------------------
const SP918_DECL: &str = "sp918decl-調べるね（spawn 宣言）";
const R918_1: &str = "r918-報告その1（resume 途中発話）";
const R918_2: &str = "r918-報告その2（resume 最終）";
const H918_ECHO: &str = "h918-echo-即時stdout";

struct ResumeContinueMock {
    emitted: std::sync::atomic::AtomicBool,
    resume_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ResumeContinueMock {
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
        // spawn ターンの ack（合成 "spawned" 結果＝tool role）→ 宣言 say（別生成・配送される）。
        if has_tool_role(&request) {
            return Ok(text_response(SP918_DECL));
        }
        // 初回（tool role 無し・1 回だけ）→ execute_shell(echo) で背景 subtask 化。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [H918_ECHO] }),
            ));
        }
        // subtask 決着後の resume ターン（tool role 無し・emitted 済み）: 本文1＋CONTINUE → 本文2。
        let n = self
            .resume_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match n {
            0 => Ok(text_response(&format!("{R918_1}\nCONTINUE"))),
            _ => Ok(text_response(R918_2)),
        }
    }
}

#[tokio::test]
async fn scenario_918_resume_turn_continue_split_delivers_both() {
    let buf = install_capture();
    let mock = Arc::new(ResumeContinueMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
        resume_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message(
        "9230",
        "SP918MARK sleep 5 して終わったら 2 回に分けて報告して",
    );

    // resume の 2 件目（R918_2）まで出るのを待つ。#918 が未修正なら R918_1 が配送されず、
    // 下の count assert が赤になる。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R918_2))
        })
        .await
    };
    assert!(
        done,
        "resume 最終報告（R918_2）が出ない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .count()
    };
    // 手順 2: spawn 宣言が 1 件配送（別生成・#916 の holding とは別経路）。
    assert_eq!(
        say_count(SP918_DECL),
        1,
        "spawn 宣言が 1 件配送されない（§13.4.2 手順 2）: {:?}",
        captured(&buf)
    );
    // 配送 2: resume 途中発話（R918_1）＋最終（R918_2）が各 1 件。現 tip で R918_1 が落ちるなら赤。
    assert_eq!(
        say_count(R918_1),
        1,
        "resume 途中発話（本文1）が配送されない（§13.4.2 手順 3・#918）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(R918_2),
        1,
        "resume 最終（本文2）が配送されない: {:?}",
        captured(&buf)
    );

    // CONTINUE 残留 0: どの say にも "CONTINUE" が出ない（§13.4.2 手順 3）。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say"
                && c.channel == CHANNEL
                && (c.body.contains(R918_1) || c.body.contains(R918_2)))
            .all(|c| !c.body.contains("CONTINUE")),
        "resume の say に CONTINUE が残留（§13.4.2 手順 3/5）: {:?}",
        captured(&buf)
    );

    // 🏁: 途中（R918_1）に 0・最終（R918_2）に 1（§13.4.2 手順 3）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let r1_mid = mid_of(R918_1).expect("R918_1 の message id");
    let r2_mid = mid_of(R918_2).expect("R918_2 の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&r1_mid),
        0,
        "🏁 が resume 途中発話に誤付与（§13.4.2 手順 3・1 件目に 🏁 なし）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&r2_mid),
        1,
        "🏁 が resume 最終に 1 件で付かない（§13.4.2 手順 3）: {:?}",
        captured(&buf)
    );

    // 保存: 宣言＋報告 2 行の計 3 行（§13.4.2 手順 4）。
    let own_speech_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        own_speech_rows, 3,
        "自 speech 保存行が宣言 1＋報告 2 の 3 行でない（§13.4.2 手順 4）: {own_speech_rows}"
    );
    // 残留 0（本文側）: 保存された自 speech の本文に CONTINUE が一切含まれない（§13.4.2 手順 3/5）。
    let continue_in_saved: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content LIKE '%CONTINUE%'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        continue_in_saved, 0,
        "保存された自 speech 本文に CONTINUE が残留（§13.4.2 手順 3/5）: {continue_in_saved}"
    );
}

// ---------------------------------------------------------------------------
// #916 レビュー要修正の観測境界【本文＋末尾 NO_REPLY＋execute_shell の 1 生成 → NO_REPLY 前の
// 本文だけ配送・保存 1・🤐 0】:
//   1 生成 = content("本文\nNO_REPLY")＋execute_shell。NO_REPLY 終端解釈（単一実装 R4）で
//   「NO_REPLY 前の本文」を holding として 1 件配送・保存する（NO_REPLY 以降は破棄・"NO_REPLY" の
//   文字は配送も保存もしない）。発話ありターンなので発端に 🤐 は付かない。
//   8d5f3ec5（`!content.contains(NO_REPLY)` の全か無かガード）では本文が holding 配送を見送られ、
//   extgate は on_tool_call を配送に使わないため本文が**配送されず消えた**（保存だけ）→ 配送 count で赤。
//   9ffa2488（terminate_at_no_reply(content).speech()）で NO_REPLY 前の本文が配送される → 緑。
// ---------------------------------------------------------------------------
const M_HNR: &str = "HOLDNRMARK";
// 本文マーカーに "NO_REPLY" を含めない（R4 は最初の NO_REPLY で終端＝マーカー内に含むと自終端する）。
const HNR_BODY: &str = "hnrbody-調べる本文（沈黙の前・holding）";
const HNR_ECHO: &str = "hnr-echo-即時stdout";

struct HoldingNoReplyShellMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for HoldingNoReplyShellMock {
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
        // spawn ack（合成 spawned 結果＝tool role）・resume（決着後）はいずれも沈黙で閉じる。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        // 初回のみ: content("本文\nNO_REPLY")＋execute_shell を同一生成で（holding＋末尾 NO_REPLY）。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(shell_with_content_response(
                &format!("{HNR_BODY}\nNO_REPLY"),
                "echo",
                &[HNR_ECHO],
            ));
        }
        // resume ターン（決着後）: 沈黙（追加投稿・保存なし）。
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn scenario_916_holding_body_before_no_reply_delivered() {
    let buf = install_capture();
    let mock = Arc::new(HoldingNoReplyShellMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9240", &format!("{M_HNR} 調べてから黙って"));

    // NO_REPLY 前の本文（HNR_BODY）が holding として 1 件配送されるまで待つ。8d5f3ec5 は配送しない → 赤。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(HNR_BODY))
        })
        .await
    };
    assert!(
        delivered,
        "NO_REPLY 前の本文が holding として配送されない（8d5f3ec5 の全か無かガードで消える）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 配送 1: NO_REPLY 前の本文だけが 1 件。
    let body_say = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(HNR_BODY))
        .count();
    assert_eq!(
        body_say,
        1,
        "NO_REPLY 前の本文が 1 件配送されていない: {:?}",
        captured(&buf)
    );
    // 残留 0: どの配送 say にも "NO_REPLY" の文字が出ない（終端以降は破棄）。
    let noreply_say = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains("NO_REPLY"))
        .count();
    assert_eq!(
        noreply_say,
        0,
        "配送 say に NO_REPLY の文字が出た（終端以降は破棄のはず）: {:?}",
        captured(&buf)
    );

    // 否定側: 発話ありターン（holding 本文を配送）なので発端 9240 に 🤐 は付かない。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9240")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "本文を配送したターンに 🤐 が誤発火: {:?}",
        captured(&buf)
    );

    // 保存 1: 自 speech は NO_REPLY 前の本文 1 行のみ（"NO_REPLY" 文字列は保存しない）。
    let own_speech_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        own_speech_rows, 1,
        "自 speech 保存が NO_REPLY 前の本文 1 行でない: {own_speech_rows}"
    );
    let noreply_saved: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content LIKE '%NO_REPLY%'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        noreply_saved, 0,
        "保存 speech に NO_REPLY の文字が残留: {noreply_saved}"
    );
}
