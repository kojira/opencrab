//! ChatGPT のサブスクで動く `gpt-5.6-luna` を、本物の `chatgpt.com/backend-api/codex/responses`
//! に対して**実際に**叩く生存確認。既定の `cargo test` を汚さないよう `#[ignore]`。
//!
//! 認証は ChatGPT の OAuth トークン（`~/.codex/auth.json`。資格情報ファイルであって codex の実装ではない）。
//! 失効していれば正直に失敗する（既定値で埋めない・§15）。
//!
//! 走らせ方:
//!   OPENCRAB_LLM_MODEL=gpt-5.6-luna \
//!     cargo test -p opencrab-app --test chatgpt_live -- --ignored --nocapture

use opencrab_app::{ChatGptProvider, HttpSseEngine};
use opencrab_port::{ChunkSink, Context, Engine};
use std::sync::Arc;

#[tokio::test]
#[ignore]
async fn chatgpt_provider_reaches_luna() {
    let base = std::env::var("OPENCRAB_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api".into());
    let model = std::env::var("OPENCRAB_LLM_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".into());
    let auth =
        std::env::var("OPENCRAB_CHATGPT_AUTH_FILE").unwrap_or_else(|_| "~/.codex/auth.json".into());
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::new(
        base.clone(),
        Box::new(
            ChatGptProvider::new(model.clone(), auth).with_reasoning_effort(Some("low".into())),
        ),
    ));
    let ctx = Context {
        rendered: "You are in a chat. Reply with a short friendly greeting in exactly three words."
            .to_string(),
        ..Default::default()
    };
    let (sink, _rx) = ChunkSink::channel();
    match engine.infer(&ctx, &sink).await {
        Ok(out) => {
            eprintln!("== base={base} model={model}");
            eprintln!(
                "== done={} effects={} tool_calls={}",
                out.done,
                out.effects.len(),
                out.tool_calls.len()
            );
            for e in &out.effects {
                eprintln!("== effect: kind={:?} content={:?}", e.kind, e.content);
            }
            assert!(out.done, "text 応答は done で終わる");
            assert!(!out.effects.is_empty(), "本文が say として返る");
        }
        Err(e) => panic!("chatgpt provider failed: {}", e.0),
    }
}
