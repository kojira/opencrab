//! LLM プロバイダの型付きエラー（#35）。
//!
//! リトライ/フォールバック方針の判断（router の `is_non_retryable_error`）が、
//! プロバイダの Display 文字列の部分一致に依存しないよう、HTTP ステータスを
//! 型として運ぶ。トレイトは anyhow::Result のままで、router 側は
//! `error.downcast_ref::<LlmError>()`（anyhow は context チェーンを遡って
//! downcast する）で分類する。

use reqwest::StatusCode;

/// プロバイダ層のエラー。リトライ分類の根拠になる情報を型で持つ。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// HTTP ステータス付きの API エラー。
    #[error("{provider} API error ({status}): {message}")]
    Http {
        provider: &'static str,
        status: u16,
        message: String,
    },
}

impl LlmError {
    /// HTTP ステータス（あれば）。
    pub fn status(&self) -> Option<u16> {
        match self {
            LlmError::Http { status, .. } => Some(*status),
        }
    }
}

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
        let llm = err.downcast_ref::<LlmError>().expect("downcast through context");
        assert_eq!(llm.status(), Some(429));
    }
}
