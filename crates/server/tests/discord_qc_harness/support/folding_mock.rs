use super::*;

pub(crate) const R930_CH: &str = "630";
pub(crate) const R930_A: &str = "R930AMARK";
pub(crate) const R930_B: &str = "R930BMARK";
pub(crate) const R930_DECL: &str = "r930decl-調べてるね（宣言・holding）";
pub(crate) const R930_BREPLY: &str = "r930breply-date の結果はこうだよ";

pub(crate) struct EyesOnReadMock {
    pub(crate) reqs: Mutex<Vec<String>>,
    pub(crate) emitted_sleep: std::sync::atomic::AtomicBool,
    pub(crate) continues: std::sync::atomic::AtomicUsize,
    /// B の独立ターン（畳み込みと二重に走る #930 第2欠陥）で返信するか。
    /// 主テスト（外形 pin）は true＝独立ターンも返信し、現 tip で B 返信 say が 2 件になる
    /// （＝「B 返信 say==1」が外形の赤）。companion（LLM 回数代理）は false＝実機どおり NO_REPLY。
    pub(crate) reply_on_independent: bool,
}

#[async_trait::async_trait]
impl LlmProvider for EyesOnReadMock {
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
        if text.contains(R930_B) {
            // 走行中ターンへ「新着メッセージ」として畳み込まれた（fold）呼び出し → B へ返信。
            if text.contains("新着メッセージ") {
                return Ok(text_response(R930_BREPLY));
            }
            // 「新着」でなく B を読む＝B が自分の独立ターンを起こした（#930 第2欠陥の二重処理）。
            // この独立ターンの started+origin(B) が返信の後に 👀 を付ける源。修正後はこの独立
            // ターン自体が起きない。主テストは返信させて外形（B 返信 say の重複）で捉え、
            // companion は実機の 07:46:00 どおり NO_REPLY で終える。
            if self.reply_on_independent {
                return Ok(text_response(R930_BREPLY));
            }
            return Ok(text_response("NO_REPLY"));
        }
        // A 初回 → execute_shell(sleep) で背景 subtask（🏁 抑制条件＝agent に running subtask）。
        if text.contains(R930_A)
            && !self
                .emitted_sleep
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "sleep", "args": ["8"] }),
            ));
        }
        // spawn 後の継続: 宣言＋CONTINUE でロックを保持したまま B の到着を待つ（畳み込み窓）。
        // 上限を設けて B が来ないときも自然終了させる（否定テスト・暴走防止）。
        let c = self
            .continues
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if c < 20 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(text_response(&format!("{R930_DECL}\nCONTINUE")))
        } else {
            Ok(text_response(FILLER))
        }
    }
}
