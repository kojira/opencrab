//! OpenAI 形式のプロバイダを、この環境のローカルの橋（`127.0.0.1:8765`・OpenAI 形式で Claude を出す）
//! に対して**実際に**叩く生存確認。既定の `cargo test` を汚さないよう `#[ignore]`。
//!
//! 走らせ方:
//!   OPENCRAB_OPENAI_BASE=http://127.0.0.1:8765 OPENCRAB_OPENAI_MODEL=claude-haiku-4-5 \
//!     cargo test -p opencrab-app --test openai_live -- --ignored --nocapture

use opencrab_app::{HttpSseEngine, OpenAiProvider};
use opencrab_port::{ChunkSink, Context, Engine};
use std::sync::Arc;

#[tokio::test]
#[ignore]
async fn openai_provider_reaches_real_model() {
    let base =
        std::env::var("OPENCRAB_OPENAI_BASE").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
    let model =
        std::env::var("OPENCRAB_OPENAI_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::new(
        base.clone(),
        Box::new(OpenAiProvider::new(model.clone(), String::new(), 128)),
    ));
    let ctx = Context {
        rendered: "Reply with a short greeting in exactly three words.".to_string(),
        ..Default::default()
    };
    let (sink, _rx) = ChunkSink::channel();
    match engine.infer(&ctx, &sink).await {
        Ok(out) => {
            eprintln!("== base={base} model={model}");
            eprintln!("== done={} effects={}", out.done, out.effects.len());
            for e in &out.effects {
                eprintln!("== effect: kind={:?} content={:?}", e.kind, e.content);
            }
            assert!(out.done, "text 応答は done で終わる");
            assert!(!out.effects.is_empty(), "本文が say として返る");
        }
        Err(e) => panic!("provider failed: {}", e.0),
    }
}
