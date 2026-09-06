use super::*;

// ==================== 内容ルーティング mock ====================

pub(crate) struct RoutedMock {
    reqs: Mutex<Vec<String>>,
}

impl RoutedMock {
    pub(crate) fn new() -> Self {
        Self {
            reqs: Mutex::new(Vec::new()),
        }
    }
    pub(crate) fn request_texts(&self) -> Vec<String> {
        self.reqs.lock().unwrap().clone()
    }
}

pub(crate) const M_SAY: &str = "SAYMARK";
pub(crate) const M_REPLY: &str = "REPLYMARK";
pub(crate) const M_REACT: &str = "REACTMARK";
// 他マーカーの部分文字列にならない独立名（"NOREPLYMARK" は "REPLYMARK" を含み誤ルートする）。
pub(crate) const M_NOREPLY: &str = "MUTEMARK";
/// 長文 say を要求するマーカー。`SAYMARK` を部分文字列に含まないので M_SAY と衝突しない。
pub(crate) const M_LONGSAY: &str = "LONGMARK";
/// 長文 say の行数。各行に `LONGSAYLINE{n}` を含め、分割後も全チャンクを識別・再構成できる。
pub(crate) const LONGSAY_LINES: usize = 200;

/// 2000 字を確実に超える複数行本文（1 行ごとに一意トークンを持つ）。
/// mock（送信元）とテスト（期待値）で同じ関数を使い、分割の再構成を厳密照合する。
pub(crate) fn long_say_body() -> String {
    (0..LONGSAY_LINES)
        .map(|i| format!("LONGSAYLINE{i:03}-これは長文分割テストの行です"))
        .collect::<Vec<_>>()
        .join("\n")
}
pub(crate) const B_SAY: &str = "saybody-alpha 通常発言だよ";
// #915: scenario_d 専用の一意 say 本文。BUFFER は binary 内で共有・累積のため、B_SAY（他シナリオ
// でも使う）だと own message id を count で pin できない。独立本文で scenario_d の say を隔離する。
pub(crate) const M_SAY_D: &str = "SAYDMARK";
pub(crate) const B_SAY_D: &str = "saydbody-delta 単発発言だよ（#915 scenario_d 専用）";
pub(crate) const B_REPLY: &str = "replybody-beta 返信本文だよ";
pub(crate) const EMOJI: &str = "👀";
pub(crate) const FILLER: &str = "fillerbody-omega";

// #900 追加マーカー（"REPLYMARK"/"MUTEMARK" 等を部分文字列に含めない独立名）。
pub(crate) const M_REPLY3: &str = "REP3MARK"; // reply×3 in one（§13 #6・reply3-in-one）
pub(crate) const M_REPLY_CONT: &str = "REPCONTMARK"; // reply＋末尾 CONTINUE（§13 #9）
pub(crate) const M_REPLY_NR: &str = "REPSILENTMARK"; // reply＋NO_REPLY（§13 #14）
pub(crate) const B_REPLY3: [&str; 3] = ["rep3-返信1", "rep3-返信2", "rep3-返信3"];
pub(crate) const B_REPLY_CONT: &str = "repcont-返信本文";
pub(crate) const B_REPLY_NR: &str = "repnr-返信本文";

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
        self.reqs.lock().unwrap().push(text.clone());
        // #900: reply＋末尾 CONTINUE の継続後（tool role あり）は最終 reply で自然終了する。
        // has_tool_role でも先に判定する（継続イテレーションはツール ack を伴う）。
        if text.contains(M_REPLY_CONT) && has_tool_role(&request) {
            return Ok(tool_call_response(
                "reply",
                serde_json::json!({"event": "e1", "text": B_REPLY_CONT}),
            ));
        }
        if !has_tool_role(&request) {
            // §13 #6: reply×3 を 1 生成に並べる（配送 3・LLM 1）。
            if text.contains(M_REPLY3) {
                return Ok(tool_calls_response(
                    B_REPLY3
                        .iter()
                        .map(|b| ("reply", serde_json::json!({"event": "e1", "text": b})))
                        .collect(),
                ));
            }
            // §13 #9: reply＋末尾 CONTINUE（1 生成目・継続する）。
            if text.contains(M_REPLY_CONT) {
                return Ok(reply_with_content_response(B_REPLY_CONT, "CONTINUE"));
            }
            // §13 #14: reply＋NO_REPLY（発話ありの沈黙終端・reply は配送される）。
            if text.contains(M_REPLY_NR) {
                return Ok(reply_with_content_response(B_REPLY_NR, "NO_REPLY"));
            }
            if text.contains(M_REPLY) {
                return Ok(tool_call_response(
                    "reply",
                    serde_json::json!({"event": "e1", "text": B_REPLY}),
                ));
            }
            if text.contains(M_REACT) {
                return Ok(tool_call_response(
                    "reaction",
                    serde_json::json!({"event": "e1", "emoji": EMOJI}),
                ));
            }
            if text.contains(M_NOREPLY) {
                // 沈黙ターン: say も tool も出さず NO_REPLY だけ返す（core は say 0・ended のみ）。
                return Ok(text_response("NO_REPLY"));
            }
            if text.contains(M_LONGSAY) {
                // 2000 字超の say（turn は plain text で閉じるので resume ループしない）。
                return Ok(text_response(&long_say_body()));
            }
            if text.contains(M_SAY_D) {
                return Ok(text_response(B_SAY_D));
            }
            if text.contains(M_SAY) {
                return Ok(text_response(B_SAY));
            }
        }
        // spawn 後の継続 / 決着後の resume。追加ツールを出さず turn を閉じる filler。
        Ok(text_response(FILLER))
    }
}
