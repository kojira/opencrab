use super::*;
use crate::message::*;
use crate::traits::LlmProvider;
use std::sync::atomic::{AtomicBool, Ordering};

struct MockProvider {
    provider_name: String,
    should_fail: AtomicBool,
}

impl MockProvider {
    fn new(name: &str, should_fail: bool) -> Self {
        Self {
            provider_name: name.to_string(),
            should_fail: AtomicBool::new(should_fail),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }
    async fn available_models(&self) -> anyhow::Result<Vec<crate::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        if self.should_fail.load(Ordering::SeqCst) {
            anyhow::bail!("mock failure");
        }
        Ok(ChatResponse {
            id: "resp-1".to_string(),
            model: request.model,
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("mock response"),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: 0,
        })
    }
}

/// 常に型付き 400 を返すモック（非リトライ分類とエラー伝播の検証用）。
struct Http400Provider;

#[async_trait::async_trait]
impl LlmProvider for Http400Provider {
    fn name(&self) -> &str {
        "http400"
    }
    async fn available_models(&self) -> anyhow::Result<Vec<crate::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        Err(crate::error::api_error(
            "Mock",
            reqwest::StatusCode::BAD_REQUEST,
            "context length exceeded",
        ))
    }
}

/// フォールバック枯渇時に最後のプロバイダエラー（型付き LlmError）が
/// 汎用文字列に握りつぶされず downcast 可能なまま返ること（#35 の end-to-end）。
#[tokio::test]
async fn test_exhausted_router_error_preserves_typed_status() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(Http400Provider));
    router.set_default_provider("http400");

    let request = ChatRequest::new("http400:some-model", vec![Message::user("hello")]);
    let err = router.chat_completion(request).await.unwrap_err();

    // サマリ context は付くが、根の LlmError は downcast で取り出せる
    assert!(err.to_string().contains("All providers failed"));
    let llm = err
        .downcast_ref::<crate::LlmError>()
        .expect("typed error must survive router exhaustion");
    assert_eq!(llm.status(), Some(400));
    assert!(llm.is_non_retryable());
}

#[test]
fn test_resolve_model_default() {
    let mut router = LlmRouter::new();
    router.set_default_provider("openai");
    let (provider, model) = router.resolve_model("gpt-4o").unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn test_resolve_model_explicit() {
    let router = LlmRouter::new();
    let (provider, model) = router.resolve_model("anthropic:claude").unwrap();
    assert_eq!(provider, "anthropic");
    assert_eq!(model, "claude");
}

#[test]
fn test_resolve_model_alias() {
    let mut router = LlmRouter::new();
    router.add_model_mapping("best", "openai:gpt-4o");
    let (provider, model) = router.resolve_model("best").unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn test_resolve_model_no_default_error() {
    let router = LlmRouter::new();
    let result = router.resolve_model("gpt-4o");
    assert!(result.is_err());
}

#[tokio::test]
async fn test_provider_success() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("openai", false)));
    router.set_default_provider("openai");

    let request = ChatRequest::new("gpt-4o", vec![Message::user("hello")]);
    let response = router.chat_completion(request).await;
    assert!(response.is_ok());
    let response = response.unwrap();
    assert_eq!(response.first_text(), Some("mock response"));
}

#[tokio::test]
async fn test_provider_fallback() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("primary", true)));
    router.add_provider(Arc::new(MockProvider::new("fallback", false)));
    router.set_default_provider("primary");
    router.set_fallback_chain(vec!["fallback".to_string()]);

    let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
    let response = router.chat_completion(request).await;
    assert!(response.is_ok());
    assert_eq!(response.unwrap().first_text(), Some("mock response"));
}

#[tokio::test]
async fn test_all_providers_fail() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("primary", true)));
    router.add_provider(Arc::new(MockProvider::new("fallback", true)));
    router.set_default_provider("primary");
    router.set_fallback_chain(vec!["fallback".to_string()]);

    let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
    let response = router.chat_completion(request).await;
    assert!(response.is_err());
}

/// primary と fallback の両方が失敗したとき、集約エラーに**両方**のプロバイダの
/// エラーが残ること（last_error だけ残すと primary=codex の原因が消えていた）。
#[tokio::test]
async fn test_aggregated_error_keeps_all_provider_errors() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("primary", true)));
    router.add_provider(Arc::new(MockProvider::new("fallback", true)));
    router.set_default_provider("primary");
    router.set_fallback_chain(vec!["fallback".to_string()]);

    let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
    let err = router.chat_completion(request).await.unwrap_err();
    // Display（{})だけでも両プロバイダの原因が見える
    let shown = format!("{err}");
    assert!(
        shown.contains("[primary]"),
        "primary エラーが消えている: {shown}"
    );
    assert!(
        shown.contains("[fallback]"),
        "fallback エラーが消えている: {shown}"
    );
    assert!(shown.contains("2 tried"), "{shown}");
}

#[tokio::test]
async fn test_skip_already_tried() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("primary", true)));
    router.set_default_provider("primary");
    // Include the primary in the fallback chain; it should be skipped
    router.set_fallback_chain(vec!["primary".to_string()]);

    let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
    let response = router.chat_completion(request).await;
    assert!(response.is_err());
}

#[test]
fn test_is_non_retryable_error_classifies_typed_errors() {
    use crate::error::api_error;
    use reqwest::StatusCode;

    // 4xx（429以外）は non-retryable
    let e400 = api_error("OpenAI", StatusCode::BAD_REQUEST, "bad param");
    assert!(LlmRouter::is_non_retryable_error(&e400));
    let e401 = api_error("Anthropic", StatusCode::UNAUTHORIZED, "invalid key");
    assert!(LlmRouter::is_non_retryable_error(&e401));

    // 429 はリトライ対象
    let e429 = api_error("OpenAI", StatusCode::TOO_MANY_REQUESTS, "slow down");
    assert!(!LlmRouter::is_non_retryable_error(&e429));

    // 5xx はリトライ対象
    let e500 = api_error("OpenAI", StatusCode::INTERNAL_SERVER_ERROR, "oops");
    assert!(!LlmRouter::is_non_retryable_error(&e500));

    // context で包まれても downcast で分類できる（anyhow はチェーンを遡る）
    let wrapped = api_error("Gemini", StatusCode::FORBIDDEN, "denied")
        .context("while calling chat_completion");
    assert!(LlmRouter::is_non_retryable_error(&wrapped));

    // 型付きでないエラー（ネットワーク・サブプロセス等）はリトライ対象
    let enet = anyhow::anyhow!("connection refused");
    assert!(!LlmRouter::is_non_retryable_error(&enet));
    // 旧形式の文字列だけを持つエラーも（もう文字列は見ないので）リトライ対象側に落ちる
    let legacy = anyhow::anyhow!("OpenAI API error (400 Bad Request): bad param");
    assert!(!LlmRouter::is_non_retryable_error(&legacy));
}

#[test]
fn test_provider_names() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("openai", false)));
    router.add_provider(Arc::new(MockProvider::new("anthropic", false)));

    let names = router.provider_names();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"anthropic"));
}

/// (a) 未登録プロバイダは選択肢に出ない。登録済みエイリアスとその解決先だけが出る。
#[test]
fn configured_model_choices_omit_unregistered_providers() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("hermit", false)));
    router.add_model_mapping("smart", "hermit:claude-sonnet");
    router.add_model_mapping("local", "ollama:llama3");
    router.add_model_mapping("openai_alias", "openai:codex");

    let choices = router.configured_model_choices();
    assert!(
        choices.contains(&"smart".to_string()),
        "登録済みエイリアスが出ない: {choices:?}"
    );
    assert!(
        choices.contains(&"hermit:claude-sonnet".to_string()),
        "登録済みの解決先が出ない: {choices:?}"
    );
    assert!(
        !choices.iter().any(|c| c.contains("openai")),
        "未登録プロバイダ openai が選択肢に出ている: {choices:?}"
    );
    assert!(
        !choices.iter().any(|c| c.contains("ollama") || c == "local"),
        "未登録プロバイダ ollama が選択肢に出ている: {choices:?}"
    );
}

/// (b) 未登録プロバイダを強制指定したら選択時に拒否する。
#[test]
fn ensure_model_configured_rejects_unregistered_provider() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("hermit", false)));

    let err = router
        .ensure_model_configured("openai:codex")
        .expect_err("未登録 openai を通してはいけない");
    let msg = format!("{err}");
    assert!(msg.contains("openai"), "{msg}");
    assert!(msg.contains("openai:codex"), "{msg}");
    assert!(msg.contains("hermit"), "構成済み一覧が無い: {msg}");
}

/// (c) 登録済みプロバイダ / そのエイリアスは通る。
#[test]
fn ensure_model_configured_accepts_registered() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MockProvider::new("hermit", false)));
    router.add_model_mapping("smart", "hermit:claude-sonnet");

    router
        .ensure_model_configured("hermit:claude-sonnet")
        .expect("登録済み provider:model を拒否してはいけない");
    router
        .ensure_model_configured("smart")
        .expect("登録済みエイリアスを拒否してはいけない");
}
