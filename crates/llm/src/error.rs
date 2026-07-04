//! LLM プロバイダの型付きエラー（#35）。
//!
//! 実体は leaf crate の [`opencrab_llm_types::LlmError`]（llm と core の両方が
//! downcast できるようにするため）。ここではプロバイダ実装向けの構築ヘルパーを提供する。

pub use opencrab_llm_types::LlmError;

use reqwest::StatusCode;

/// プロバイダの `bail!("... API error ({status}): {msg}")` 置き換え用ヘルパー。
///
/// メッセージ抽出（JSON error.message / raw text）は呼び出し側の責務のまま。
pub fn api_error(
    provider: &'static str,
    status: StatusCode,
    message: impl Into<String>,
) -> anyhow::Error {
    anyhow::Error::new(LlmError::Http {
        provider,
        status: status.as_u16(),
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_format_matches_previous_convention() {
        let err = api_error("OpenAI", StatusCode::BAD_REQUEST, "bad param");
        // 旧形式 "OpenAI API error (400 Bad Request): ..." に近い可読形式を保つ
        assert_eq!(err.to_string(), "OpenAI API error (400): bad param");
    }

    #[test]
    fn downcast_survives_context_chain() {
        let err = api_error("Ollama", StatusCode::TOO_MANY_REQUESTS, "slow down")
            .context("while calling chat_completion");
        let llm = err
            .downcast_ref::<LlmError>()
            .expect("downcast through context");
        assert_eq!(llm.status(), Some(429));
        assert!(!llm.is_non_retryable());
    }
}
