use super::support::*;

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 16【反復上限（max_iterations）到達 → 最後に配送した投稿に 1】: 常に
// 「本文＋CONTINUE」を返し続けるターンは depth0 の上限（process.rs:1583・30）で打ち切られる。
// 打ち切られた最終生成の最後の投稿（＝最後に配送した say）にだけ 🏁 1・他 0・総数 1。
// 現 tip は say 配送ごとに 🏁 を付けるため全 say に付く → **赤**。上限は既存ハードコードを実走
// （dry-run なので高速・test-only の上限注入は作らない・統括裁定）。
// ---------------------------------------------------------------------------

const ML_PREFIX: &str = "mlsay-iteration-";

struct AlwaysContinueMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for AlwaysContinueMock {
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
        // 常に「本文＋末尾 CONTINUE」で継続 → 上限まで回る。各本文は一意（buffer 順で最後を特定）。
        Ok(text_response(&format!("{ML_PREFIX}{n:03}\nCONTINUE")))
    }
}

#[tokio::test]
async fn scenario_915_max_iterations_flag_only_on_last_delivered_say() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(AlwaysContinueMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9190", "MLMARK ずっと続けて（上限まで）");

    // 上限打ち切りまで走る。LLM 呼び出しが 30 回に達する（depth0 上限）まで待つ。
    let looped = {
        let mock = mock.clone();
        wait_until(move || mock.calls.load(Ordering::SeqCst) >= 30).await
    };
    assert!(
        looped,
        "上限まで回らない（LLM calls={}）",
        mock.calls.load(Ordering::SeqCst)
    );
    // 打ち切り後の決着（ended）猶予。
    tokio::time::sleep(Duration::from_millis(800)).await;

    // このターンの mlsay say を buffer 順（配送順）で集める。最後の 1 つが「最後に配送した投稿」。
    let ml_says: Vec<String> = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(ML_PREFIX))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .collect();
    assert!(
        ml_says.len() >= 2,
        "上限ケースの say が 2 通以上出ていない（実測 {}）: {:?}",
        ml_says.len(),
        captured(&buf)
    );
    let last_say = ml_says.last().unwrap().clone();

    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 🏁 は最後に配送した say に 1・総数 1（§13.3.6 row 16）。現 tip は全 say に付く → 赤。
    assert_eq!(
        completed_on(&last_say),
        1,
        "🏁 が最後に配送した say に 1 件で付かない（§13.3.6 row 16）: {:?}",
        captured(&buf)
    );
    let total: usize = ml_says.iter().map(|id| completed_on(id)).sum();
    assert_eq!(
        total,
        1,
        "上限ターンの 🏁 総数が 1 でない（全 say に付いている）: mlsays={}, 🏁総数={}",
        ml_says.len(),
        total
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 10【本文＋query ツール（holding・spawn）→ 宣言 0 ／ resume 完了報告 → 1】:
// **実物の execute_shell**（echo・即時決着＝date 相当）を呼ぶターン。execute_shell は inline 集合に
// 無いので背景 subtask 化され（#152/#671）、dispatch 直後の継続で宣言（holding）say を投稿する。
// subtask 決着（実 echo → settle）後の resume ターンで完了報告 say を投稿。🏁 は宣言 say には付かず
// （進行中）、resume 報告 say に 1・総数 1。統括裁定: 照会クラス常時 detach なので「date 単独」も実機は
// この execute_shell→spawned→resume 経路。現 tip は say 配送ごとに 🏁 → 宣言にも付く → **赤**（総数 2）。
// 偽ツールは作らず、echo のみ許可した実 shell 設定で実走する。
// ---------------------------------------------------------------------------
const SP_ECHO: &str = "specho-shell-stdout-即時";
const SP_DECL: &str = "spdecl-調べてるね（宣言・holding）";
const SP_REPORT: &str = "spreport-結果はこうだった（完了報告投稿）";

struct ShellSpawnResumeMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for ShellSpawnResumeMock {
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
        // (2) dispatch 直後の継続（合成 "spawned" 結果＝tool role）→ 宣言 say（holding）でターンを閉じる。
        if has_tool_role(&request) {
            return Ok(text_response(SP_DECL));
        }
        // (1) 初回（tool role 無し・1 回だけ）→ 実 execute_shell（echo）を呼ぶ＝背景 subtask 化。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [SP_ECHO] }),
            ));
        }
        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 完了報告 say。
        Ok(text_response(SP_REPORT))
    }
}

#[tokio::test]
async fn scenario_915_spawned_declaration_no_flag_resume_report_gets_flag() {
    let buf = install_capture();
    let mock = Arc::new(ShellSpawnResumeMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    // echo を実走させるため shell を有効化（tools_config は Arc<RwLock> 共有で runtime に即反映）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9200", "SPSHELLMARK 調べて終わったら教えて");

    // 宣言 say（SP_DECL）と resume 完了報告 say（SP_REPORT）が両方出るまで待つ。
    let both = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            caps.iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SP_DECL))
                && caps
                    .iter()
                    .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SP_REPORT))
        })
        .await
    };
    assert!(
        both,
        "宣言 say と resume 完了報告 say が揃わない（subtask/resume 未達）: {:?}",
        captured(&buf)
    );
    // 決着後の 🏁 付与猶予。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let decl_mid = mid_of(SP_DECL).expect("宣言 say の message id");
    let report_mid = mid_of(SP_REPORT).expect("完了報告 say の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 宣言 say には 🏁 は付かない（進行中＝subtask 未決着）。現 tip は付く → 赤。
    assert_eq!(
        completed_on(&decl_mid),
        0,
        "🏁 が spawned 宣言 say に誤付与（進行中は付けない・§13.3.6）: {:?}",
        captured(&buf)
    );
    // resume 完了報告 say には 🏁 1。
    assert_eq!(
        completed_on(&report_mid),
        1,
        "🏁 が resume 完了報告 say に 1 件で付かない（§13.3.6・§13.3.4）: {:?}",
        captured(&buf)
    );
    // 2 say（宣言・報告）合計で 🏁 は 1（宣言に付く現 tip は総数 2 → 赤）。
    let total = completed_on(&decl_mid) + completed_on(&report_mid);
    assert_eq!(
        total,
        1,
        "spawned→resume の 🏁 総数が 1 でない（宣言に誤付与）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.1【別 session の subtask 進行中 → 本ターンの投稿に 🏁 0（エージェント単位 idle）】:
// チャンネル A（session A）で subtask を起こし**保留**（決着させない）。その状態でチャンネル B
// （session B）へ通常メッセージを送り say を投稿。エージェントに未決着 subtask があるので B の say は
// idle でない＝🏁 0。現 tip は say 配送ごと（session を見ない）に 🏁 → B の say に付く → **赤**。
// §13.3.1 案E（agent 単位）確定・§13.3.5 は agent-scope 集計を要ビルド検証と明記。
// 専用チャンネル 602（spawner）/603（plain）で他テストと分離。
// ---------------------------------------------------------------------------
const CHANNEL_XA: &str = "602";
const CHANNEL_XB: &str = "603";
const M_XSPAWN: &str = "XSPAWNMARK";
const M_XPLAIN: &str = "XSPLAINMARK";
const XS_DECL: &str = "xsdecl-Aの宣言投稿";
const XS_PLAIN: &str = "xsplain-Bの通常発言";

struct ShellSleepCrossMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for ShellSleepCrossMock {
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
        // (D) チャンネル B の通常ターン: say を返す。
        if text.contains(M_XPLAIN) {
            return Ok(text_response(XS_PLAIN));
        }
        // (B) チャンネル A の execute_shell 後（tool_result 有り）: 宣言 say（holding）。
        if has_tool_role(&request) {
            return Ok(text_response(XS_DECL));
        }
        // (A) チャンネル A の初回: 実 execute_shell（sleep＝遅延決着）で背景 subtask を起こし保留。
        // emitted で 1 回だけ（resume ターンで再発行して無限ループしないため）。
        if text.contains(M_XSPAWN) && !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "sleep", "args": ["8"] }),
            ));
        }
        Ok(text_response("xsfiller"))
    }
}

#[tokio::test]
async fn scenario_915_other_session_subtask_in_progress_no_flag() {
    let buf = install_capture();
    let mock = Arc::new(ShellSleepCrossMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    // sleep を実走させるため shell を有効化（echo・sleep を許可）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    // チャンネル A（session A）: execute_shell(sleep) で subtask を起こして保留。
    let fixture_a = Fixture::new();
    let _client_a = wire_instance_on_channel(&core, &fixture_a, CHANNEL_XA).await;
    // チャンネル B（session B）: 通常ターン。
    let fixture_b = Fixture::new();
    let _client_b = wire_instance_on_channel(&core, &fixture_b, CHANNEL_XB).await;

    fixture_a.append_message_ch("9210", CHANNEL_XA, &format!("{M_XSPAWN} 調べて"));

    // A の宣言 say が出る＝subtask を起こして走行中（保留中）になったことの proxy。
    let a_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL_XA && c.body.contains(XS_DECL))
        })
        .await
    };
    assert!(
        a_ready,
        "A の宣言 say が出ない（subtask 未起動）: {:?}",
        captured(&buf)
    );

    // subtask 保留中に B へ通常メッセージ → say を投稿。
    fixture_b.append_message_ch("9211", CHANNEL_XB, &format!("{M_XPLAIN} 2 足す 2 は?"));
    let b_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL_XB && c.body.contains(XS_PLAIN))
        })
        .await
    };
    assert!(b_ready, "B の通常 say が出ない: {:?}", captured(&buf));
    // false-red 防止（統括指摘）: B の 🏁 判定は B の activity ended（B の say 直後）で行われる。
    // その時点で A の subtask（sleep 8）が走行中であることを明示確認する。走行中でなければ sleep 窓を
    // 過ぎており、B が正しく 🏁 を得た（＝テスト前提崩れ）ので、assert ではなくこの確認で弾く。
    assert!(
        core.state
            .subtask_registries
            .has_running_for_agent(AGENT_ID),
        "B 判定時点で A の subtask が走行中でない（sleep 窓を過ぎた・テスト前提崩れ）: {:?}",
        captured(&buf)
    );
    // 決着（🏁 付与）の猶予。この間 subtask は sleep 8 で保留のまま。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let b_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL_XB && c.body.contains(XS_PLAIN))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("B の say の message id");
    let completed_on_b = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == b_mid
        })
        .count();

    // エージェントに未決着 subtask（session A）があるので、B の say には 🏁 を付けない（§13.3.1 案E）。
    // 現 tip は say 配送ごとに付ける（session を見ない）ため B に付く → 赤。
    assert_eq!(
        completed_on_b,
        0,
        "🏁 が別 session の subtask 進行中に B の say へ誤付与（agent 単位 idle・§13.3.1）: {:?}",
        captured(&buf)
    );
    // sleep 3 は自然終了するので後片付け不要（B の判定はその窓の中で確定済み）。
}

// ---------------------------------------------------------------------------
// #915 typing【最終投稿後の入力中停止】: 発話ターンで activity ended を受けたら typing keepalive
// が止まり、失効間隔（8 秒）を跨いでも typing の再送出が 0 であることを観測境界（dry-run
// kind="typing" のキャプチャ増分）で確定する。単一スレッド実行なので、この待機中に typing が
// 増えるのはこのターンの keepalive だけ（他テストは走らない）。
// ---------------------------------------------------------------------------
const TY_SAY: &str = "tysay-入力中停止確認の本文";

struct SingleSayThenIdleMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SingleSayThenIdleMock {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(text_response(TY_SAY))
    }
}

#[tokio::test]
async fn scenario_915_typing_stops_after_turn_end() {
    let buf = install_capture();
    let mock = Arc::new(SingleSayThenIdleMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    // typing は capture に scope key（message）を持たず、CI は並列・BUFFER 共有なので、他テストが
    // 使わない専用チャンネル（CHANNEL_TY）へ束ねて typing を隔離する。
    let _client = wire_instance_on_channel(&core, &fixture, CHANNEL_TY).await;

    fixture.append_message_ch(
        "9154",
        CHANNEL_TY,
        &format!("{M_SAY} 入力中がターン後に止まるか"),
    );

    // 最終 say（TY_SAY）の own id に 🏁 が付く＝activity ended を受けた（typing も ended で停止）まで待つ。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL_TY && c.body.contains(TY_SAY))
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
        done,
        "最終 say＋🏁（＝ended 到達）が観測できない: {:?}",
        captured(&buf)
    );

    // ended 到達後、専用チャンネルの typing キャプチャ数を基準化し、失効間隔
    // （TYPING_REFRESH_INTERVAL=8 秒）を跨いで待つ。このチャンネルはこのターンだけが使うので、
    // 停止していれば増分 0・停止漏れなら keepalive が 8 秒後に再送出して増える（並列でも堅牢）。
    let before = count_kind_on_channel(&buf, "typing", CHANNEL_TY);
    tokio::time::sleep(Duration::from_millis(9000)).await;
    let after = count_kind_on_channel(&buf, "typing", CHANNEL_TY);
    assert_eq!(
        after, before,
        "activity ended 後に typing が再送出された（入力中が止まらない）: before={before} after={after}"
    );
}

// ---------------------------------------------------------------------------
// §13.1 g【reaction のみ → #6 と同じ】: reaction のみ（say 0）のターン → reaction 配送・
// 🤐 発端に付けない（発話がある）。現 tip: 最終本文空を沈黙とみなし 🤐 → 赤（#900 の reaction 版）。
// §13 #6 の 🤐 契約を reaction で固定。
// ---------------------------------------------------------------------------
const M_REACT_ONLY: &str = "REACTONLYMARK";
const REACT_EMOJI: &str = "🎉";

struct ReactionOnlyMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReactionOnlyMock {
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
        if !has_tool_role(&request) && request_text(&request).contains(M_REACT_ONLY) {
            return Ok(tool_call_response(
                "reaction",
                serde_json::json!({"event": "e1", "emoji": REACT_EMOJI}),
            ));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn audit_s13_1g_reaction_only_turn_gets_no_muted_reaction() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReactionOnlyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message(
        "1309",
        &format!("{M_REACT_ONLY} これにリアクションだけして"),
    );

    // agent の reaction（kind=reaction）が発端 1309 に出るまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.message == "1309")
        })
        .await
    };
    assert!(delivered, "reaction が配送されない: {:?}", captured(&buf));

    // 決着の猶予（🤐 判定は決着時）。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 🤐 なし: 発話（reaction）があったターンなので発端 1309 に 🤐 は付かない（現 tip は付く → 赤）。
    let muted = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "1309")
        .count();
    assert_eq!(
        muted,
        0,
        "reaction があったターンに 🤐 が誤発火（#900・§13 #6 の reaction 版）: {:?}",
        captured(&buf)
    );

    // §13.2: reaction のみのターンは自分の「投稿」が無い（reaction は発話 op だが say/reply の
    // ような本文投稿ではない）ので 🏁 は付かない（発端 1309 への誤付与を総数 0 で pin）。
    let completed_on_1309 = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "1309"
        })
        .count();
    assert_eq!(
        completed_on_1309,
        0,
        "reaction のみのターンに 🏁 が誤発火（§13.2）: {:?}",
        captured(&buf)
    );
    assert!(
        mock.calls.load(Ordering::SeqCst) >= 1,
        "reaction ターンの LLM 呼び出しが走らない"
    );
}
