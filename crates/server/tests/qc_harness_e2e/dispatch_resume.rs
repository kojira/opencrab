// ==================== (shell) execute_shell の stdout が resume 会話に現れる ====================
//
// くらぶ暴走の根因回帰。`execute_shell` は inline 集合に無いため常に背景 subtask 化され
// （#152/#671）、その完了本文（＝ツール結果 JSON）は会話再構成で参照へ畳まれていた（#713）。
// #713 の「同ターン内は本文がモデルに渡る」前提は inline ツールでのみ成立し、execute_shell は
// 同ターン往復が無いので、畳むと stdout をどのターンでも読めず、モデルが出力を取り直そうと
// 待機宣言を連投した（実機で確認）。修正で exit_code を持つ結果は畳まず stdout を会話へ残す。
//
// このハーネスは実配線（実 dispatch 判定 → 実 execute_shell = 実 echo → 実 settle_completed →
// 実 resume）を通す。ピン: **resume ターンの会話（LLM リクエスト本文）に echo の stdout が現れる**
// ——修正前はここで落ちる（参照へ畳まれ stdout が消える）。

const M_SHELL: &str = "SHELLQC-MARK 東京の天気を調べて教えて";
/// echo で実際に出力させる stdout。マーカーとも ack/done 本文とも部分一致しない。
const SHELL_STDOUT: &str = "SHELLOUT-tenki 晴れ 28度 くもり所により雨";
const B_SHELL_ACK: &str = "shellack-epsilon 調べてるよ、ちょっと待ってね";
const B_SHELL_DONE: &str = "shelldone-zeta 東京は晴れ 28度だよ";

/// execute_shell の段階応答 mock。全リクエスト本文を記録し、resume ターンの本文を surface する。
struct ShellMock {
    shell_emitted: AtomicBool,
    shell_calls: AtomicUsize,
    /// resume（決着後の再開ターン）で会話に渡された本文。ピンの検証対象。
    resume_text: Mutex<Option<String>>,
}

impl ShellMock {
    fn new() -> Self {
        Self {
            shell_emitted: AtomicBool::new(false),
            shell_calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ShellMock {
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

        // (2) dispatch 直後の継続イテレーション（合成 "spawned" 結果 = tool role）→ ack say で
        //     ターンを閉じる。ここで背景 subtask（echo）が走る。
        if has_tool_role(&request) {
            return Ok(text_response(B_SHELL_ACK));
        }
        // (1) 初回メンション（tool role 無し・最初の 1 回だけ）→ execute_shell を呼ぶ。
        //     opencrab は execute_shell を inline 化しないので背景 subtask へ回る。
        if !self.shell_emitted.swap(true, Ordering::SeqCst) {
            self.shell_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [SHELL_STDOUT] }),
            ));
        }
        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 会話本文を捕まえて
        //     完了報告 say で閉じる。**この text に echo の stdout が含まれていること**がピン。
        *self.resume_text.lock().unwrap() = Some(text);
        Ok(text_response(B_SHELL_DONE))
    }
}

/// echo だけを許可した shell 有効の tools 設定。
fn shell_enabled_tools_config() -> opencrab_actions::tools::ToolsConfig {
    opencrab_actions::tools::ToolsConfig {
        enabled: true,
        shell: Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["echo".to_string()],
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn scenario_shell_stdout_survives_into_resume_turn() {
    let buf = install_capture();
    let mock = Arc::new(ShellMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // execute_shell を実際に走らせるため shell を有効化（echo のみ許可）。tools_config は
    // Arc<RwLock> 共有なので serve_uds へ渡った runtime にも即時反映される。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "d1".repeat(32);
    fixture.append_line(&mention_event(&ev, M_SHELL));

    // 決着後の完了報告 say が出るまで待つ（= resume ターンまで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_SHELL_DONE).is_some()).await
    };
    assert!(
        done,
        "決着後の resume 完了報告が出ない（execute_shell の背景 subtask が resume まで到達しない）: {:?}",
        captured(&buf)
    );

    // ピン: resume ターンの会話本文に echo の stdout が現れる（修正前はここで落ちる）。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains(SHELL_STDOUT),
        "resume 会話に execute_shell の stdout が無い（畳まれた＝くらぶ暴走の根因）。\n\
         resume 本文（先頭 2000 字）: {:.2000}",
        resume_text
    );

    // 行動系: execute_shell の dispatch はちょうど 1 回（再取得＝取り直しループをしていない）。
    assert_eq!(
        mock.shell_calls.load(Ordering::SeqCst),
        1,
        "execute_shell が複数回 dispatch された（取り直しループ）"
    );
    // ack say（「待ってね」相当）は高々 1 回（待機宣言を連投しない）。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_SHELL_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

// ========== (#880) exit_code 無しの dispatch ツールの結果本文が resume 会話に現れる ==========
//
// #877 の E2E（execute_shell の stdout が resume 会話に残る）を **exit_code 無しの dispatch ツール**
// へ拡張した回帰（設計 §6 A2 の第二ケース）。`ws_write` は `CORE_DISPATCHABLE_ACTIONS` にあり
// 常に背景 subtask 化される・戻り値に `exit_code` を持たない。#877 は exit_code を持つ結果だけ
// 畳みを撤回したので、ws_write のような exit_code 無し dispatch ツールの結果本文は「結果 N 文字」へ
// 畳まれ、切り離した subtask の結果を resume がどのターンでも読めず再 dispatch の燃料になっていた
// （症状B）。#880 で exit_code の有無に関わらず本文（payload）を会話へ残す。
//
// このハーネスは実配線（実 dispatch 判定 → 実 ws_write → 実 settle_completed → 実 resume）を通す。
// ピン: **resume ターンの会話本文に ws_write の payload（書いた path）が現れる**——修正前はここで
// 落ちる（「結果 N 文字」へ畳まれ path が消える）。加えて再 dispatch 無し（有界）を固定する。

/// mock agent が ws_write に渡す（＝結果 payload に現れる）path。ack/done 本文と部分一致しない。
const WS_WRITE_PATH: &str = "wsqc-880-notes.md";
const M_WSWRITE: &str = "WSWRITEQC-MARK 設計メモを保存して";
const B_WSWRITE_ACK: &str = "wswriteack-eta 保存するね、ちょっと待ってて";
const B_WSWRITE_DONE: &str = "wswritedone-theta 保存したよ";

/// ws_write の段階応答 mock。resume ターンの会話本文を surface する。
struct WsWriteMock {
    emitted: AtomicBool,
    calls: AtomicUsize,
    resume_text: Mutex<Option<String>>,
}

impl WsWriteMock {
    fn new() -> Self {
        Self {
            emitted: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for WsWriteMock {
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
        // (2) dispatch 直後の継続イテレーション（合成 "spawned" 結果 = tool role）→ ack say で閉じる。
        if has_tool_role(&request) {
            return Ok(text_response(B_WSWRITE_ACK));
        }
        // (1) 初回メンション → ws_write を呼ぶ（inline 化されないので背景 subtask へ回る）。
        if !self.emitted.swap(true, Ordering::SeqCst) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "ws_write",
                serde_json::json!({ "path": WS_WRITE_PATH, "content": "# 設計メモ\n本文" }),
            ));
        }
        // (3) 決着後の resume ターン → 会話本文を捕まえて完了報告 say で閉じる。
        //     **この text に ws_write の path（payload）が含まれていること**がピン。
        *self.resume_text.lock().unwrap() = Some(text);
        Ok(text_response(B_WSWRITE_DONE))
    }
}

#[tokio::test]
async fn scenario_no_exit_code_dispatch_result_survives_into_resume_turn() {
    let buf = install_capture();
    let mock = Arc::new(WsWriteMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e1".repeat(32);
    fixture.append_line(&mention_event(&ev, M_WSWRITE));

    // 決着後の完了報告 say が出るまで待つ（= resume ターンまで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_WSWRITE_DONE).is_some()).await
    };
    assert!(
        done,
        "決着後の resume 完了報告が出ない（ws_write の背景 subtask が resume まで到達しない）: {:?}",
        captured(&buf)
    );

    // ピン: resume ターンの会話本文に ws_write の payload（書いた path）が現れる（修正前は畳まれて消える）。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains(WS_WRITE_PATH),
        "resume 会話に exit_code 無し dispatch ツール（ws_write）の結果本文が無い（畳まれた＝症状B の燃料）。\n\
         resume 本文（先頭 2000 字）: {:.2000}",
        resume_text
    );

    // 行動系: ws_write の dispatch はちょうど 1 回（結果を読めるので取り直しループをしない）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        1,
        "ws_write が複数回 dispatch された（取り直しループ）"
    );
    // ack say（待機宣言）は高々 1 回（連投しない）。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_WSWRITE_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

// ============ (shell 大出力) offload → ws_read 読み戻し → 回答（再帰ループ閉包 E2E） ============
//
// #856 発見3 の回帰。#877 の E2E（小出力が resume 会話に verbatim で残る）を **大出力版**へ拡張し、
// 「大結果を畳む→レシピで読み戻す→その読み出しがまた畳まれて読めないループ」が閉じていることを
// 実配線で固定する:
//
//   execute_shell が大出力（>2,500 tok）を返す → 会話へ届く前に workspace/tmp へ offload され
//   回収レシピ付き notice に化ける（#551）→ resume ターンで mock agent が notice の tmp パスを
//   **実 ws_read**（inline）で読み戻す → 読めた本文で回答し、**同じ execute_shell を再実行しない**。
//
// ピン: (a) resume 会話に offload notice（tmp パス）が現れる、(b) mock が ws_read で読んだ本文に
// 大出力のマーカーが含まれる、(c) execute_shell の dispatch はちょうど 1 回・ws_read も 1 回
// （読み戻しが再 offload → 再 ws_read の無限ループになっていない）、(d) ack say は高々 1 回。

/// resume 会話に載る offload notice のマーカー（大出力の先頭行）。ws_read で読み戻すと
/// ws_read 結果の tool メッセージにこの文字列が現れる＝実際に読めている証拠。
const BIG_OUT_MARK: &str = "BIGOUT-marker 東京の天気 晴れ 28度";
const M_BIG_SHELL: &str = "BIGSHELLQC-MARK 大きな出力のコマンドを実行して結果を教えて";
const B_BIG_ACK: &str = "bigack-eta 実行中、ちょっと待ってね";
const B_BIG_DONE: &str = "bigdone-theta 読み終わった、東京は晴れ 28度だよ";

/// echo に渡す大出力（>2,500 tok）。実改行を含む単一 arg なので echo がそのまま複数行で吐く。
/// 先頭行に [`BIG_OUT_MARK`]。offload 閾値を確実に超える大きさにする。
fn big_shell_payload() -> String {
    let body =
        "src/foo.rs:99:    let value = compute(argument, more, and_more); // dense output row";
    std::iter::once(BIG_OUT_MARK.to_string())
        .chain(std::iter::repeat_n(body.to_string(), 400))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tool ロールのメッセージ本文を連結（合成 spawned 結果か ws_read 結果本文かの判別に使う）。
fn tool_role_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 大出力 shell の段階応答 mock。offload notice を読み、レシピどおり ws_read で読み戻す。
struct BigShellMock {
    shell_emitted: AtomicBool,
    ws_read_emitted: AtomicBool,
    shell_calls: AtomicUsize,
    ws_read_calls: AtomicUsize,
    /// resume ターンで会話に渡された本文（offload notice を含むはず）。
    resume_text: Mutex<Option<String>>,
    /// ws_read が返した本文（マーカーを含むはず＝実際に読めた証拠）。
    read_back_text: Mutex<Option<String>>,
}

impl BigShellMock {
    fn new() -> Self {
        Self {
            shell_emitted: AtomicBool::new(false),
            ws_read_emitted: AtomicBool::new(false),
            shell_calls: AtomicUsize::new(0),
            ws_read_calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
            read_back_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for BigShellMock {
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
            let tool_text = tool_role_text(&request);
            // ws_read の結果（マーカー入り本文）が返ってきた → 読めた本文で回答。
            // **同じ execute_shell は再実行しない**（回答して閉じる）。
            if tool_text.contains(BIG_OUT_MARK) {
                *self.read_back_text.lock().unwrap() = Some(tool_text);
                return Ok(text_response(B_BIG_DONE));
            }
            // それ以外（dispatch 直後の合成 `spawned` 結果）→ ack でターンを閉じる。
            return Ok(text_response(B_BIG_ACK));
        }

        // (1) 初回メンション（tool role 無し・最初の 1 回）→ 大出力 execute_shell を呼ぶ。
        if !self.shell_emitted.swap(true, Ordering::SeqCst) {
            self.shell_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [big_shell_payload()] }),
            ));
        }

        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目）→ 会話の offload notice から
        //     tmp パスを取り出し、**レシピどおり ws_read で読み戻す**（inline 実行）。
        if !self.ws_read_emitted.swap(true, Ordering::SeqCst) {
            *self.resume_text.lock().unwrap() = Some(text.clone());
            // notice の単独 rel トークン（`tmp/…txt`）を拾う。回収レシピの複合トークン
            // （`grep -n <pattern> tmp/…` 等）ではなく、backtick で囲われた素のパスを取る。
            let rel = text
                .split('`')
                .find(|t| t.starts_with("tmp/") && t.ends_with(".txt"))
                .unwrap_or("tmp/MISSING.txt")
                .to_string();
            self.ws_read_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "ws_read",
                serde_json::json!({ "path": rel, "start_line": 1 }),
            ));
        }

        // フォールバック（想定外の追加ターン）→ 回答で閉じる。
        Ok(text_response(B_BIG_DONE))
    }
}

#[tokio::test]
async fn scenario_shell_big_output_offload_read_back_loop_closed() {
    let buf = install_capture();
    let mock = Arc::new(BigShellMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // echo のみ許可（大出力 arg を吐かせる）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e2".repeat(32);
    fixture.append_line(&mention_event(&ev, M_BIG_SHELL));

    // 読み戻し後の完了報告 say が出るまで待つ（= offload→ws_read→回答まで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_BIG_DONE).is_some()).await
    };
    assert!(
        done,
        "読み戻し後の完了報告が出ない（大出力の offload→ws_read→回答チェーンが閉じない）: {:?}",
        captured(&buf)
    );

    // (a) resume 会話に offload notice（tmp パス）が現れる。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains("Tool result withheld"),
        "resume 会話に offload notice が無い（大出力が畳まれていない）:\n{:.600}",
        resume_text
    );
    assert!(
        resume_text.contains("tmp/") && resume_text.contains("ws_read"),
        "notice に読める handle（tmp パス＋ws_read レシピ）が無い:\n{:.600}",
        resume_text
    );

    // (b) mock が ws_read で読み戻した本文に大出力のマーカーがある（実際に読めている）。
    let read_back = mock
        .read_back_text
        .lock()
        .unwrap()
        .clone()
        .expect("ws_read の結果ターンが実行されていない");
    assert!(
        read_back.contains(BIG_OUT_MARK),
        "ws_read で読み戻した本文にマーカーが無い（回収レシピが機能していない）:\n{:.600}",
        read_back
    );
    // 読み戻した ws_read 結果自体は再 offload されていない（notice ではなく実本文が来ている）。
    assert!(
        !read_back.contains("Tool result withheld"),
        "ws_read 結果がまた offload された＝読み戻しがループする（#856 発見3 が閉じていない）:\n{:.600}",
        read_back
    );

    // (c) execute_shell の dispatch はちょうど 1 回・ws_read もちょうど 1 回
    //     （読み戻しが再 offload→再 ws_read の無限ループになっていない）。
    assert_eq!(
        mock.shell_calls.load(Ordering::SeqCst),
        1,
        "execute_shell が複数回 dispatch された（取り直しループ）"
    );
    assert_eq!(
        mock.ws_read_calls.load(Ordering::SeqCst),
        1,
        "ws_read が複数回走った（読み戻しが再 offload→再 ws_read のループに入った）"
    );

    // (d) ack say（待機宣言）は高々 1 回。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_BIG_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

