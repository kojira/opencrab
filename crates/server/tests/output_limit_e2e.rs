//! #676（方針3・条件3）: chatgpt 経路の「incomplete 応答 → ターン失敗」を end-to-end に近い
//! 形で 1 本固定する。
//!
//! chatgpt（Responses API）は max_output_tokens を送れないため、出力上限のモデル登録では
//! 守れない。代わりに、モデル内部既定に当たって打ち切られた（incomplete）応答を **本物の
//! `ChatGptProvider::parse_response`** が finish_reason=Length にし、それを **本物の
//! `SkillEngine`** が fail loud でターン失敗にする——この 2 段の連結が in-use の
//! chatgpt:gpt-5.6-sol を守る実質的な防衛線。ここではその連結を通しで検証する。

use async_trait::async_trait;
use serde_json::Value;

use opencrab_core::{
    ActionExecutor, ActionResult, ChatRequest, ChatResponse, FunctionDefinition, LlmClient,
    SkillEngine,
};
use opencrab_llm::providers::ChatGptProvider;

/// chatgpt が打ち切った応答（parse_response の出力）を 1 度だけ返す LLM。
struct FixedLlm {
    response: std::sync::Mutex<Option<ChatResponse>>,
}

#[async_trait]
impl LlmClient for FixedLlm {
    async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.response
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("no more responses"))
    }
}

/// ツールを持たない no-op executor。
struct NoopExecutor;

#[async_trait]
impl ActionExecutor for NoopExecutor {
    async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
        ActionResult {
            success: false,
            data: serde_json::json!(null),
            error: Some(format!("unexpected tool call: {name}")),
        }
    }
    fn list_tools(&self) -> Vec<FunctionDefinition> {
        Vec::new()
    }
}

#[tokio::test]
async fn chatgpt_incomplete_response_fails_the_turn() {
    // 1) 本物の chatgpt パーサで、出力上限による打ち切り（incomplete + max_output_tokens）を
    //    finish_reason=Length に変換する。
    let sse = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"これから報告を書\"}\n",
        "\n",
        "event: response.incomplete\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-inc\",",
        "\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},",
        "\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":4096,",
        "\"total_tokens\":4106}}}\n",
        "\n",
    );
    let response = ChatGptProvider::new()
        .parse_response(sse, "gpt-5.6-sol")
        .expect("parse failed");
    assert_eq!(
        response.choices[0].finish_reason,
        Some(opencrab_llm::message::FinishReason::Length),
        "chatgpt パーサが incomplete を Length にしていない"
    );

    // 2) その応答を本物の SkillEngine に通すと、ターンが fail loud で失敗する。
    let llm = FixedLlm {
        response: std::sync::Mutex::new(Some(response)),
    };
    let engine = SkillEngine::new(Box::new(llm), Box::new(NoopExecutor), 10);
    let err = engine
        .run("system", "調査して報告して", "chatgpt:gpt-5.6-sol")
        .await
        .expect_err("chatgpt の incomplete 応答はターンを失敗させねばならない");
    let msg = err.to_string();
    assert!(
        msg.contains("切り捨て"),
        "エラー文言が切り捨てを明示していない: {msg}"
    );
}
