//! operator Bearer。値を Debug / log / detail に出さない。

use std::fmt;

use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;

use crate::error::{ErrorCode, GateError};

const ENV_NAME: &str = "OPENCRAB_GATE_OPERATOR_TOKEN";

/// redacted memory 型。
pub struct OperatorToken {
    expected: String,
}

impl fmt::Debug for OperatorToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OperatorToken(redacted)")
    }
}

impl OperatorToken {
    pub fn from_bytes(expected: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
        }
    }

    /// startup に 1 回読み、直後に `remove_var` する。
    pub fn take_from_env() -> Self {
        let expected = std::env::var(ENV_NAME).unwrap_or_default();
        std::env::remove_var(ENV_NAME);
        Self { expected }
    }

    pub fn is_empty(&self) -> bool {
        self.expected.is_empty()
    }

    /// 最大長まで XOR。長さ差も accumulator。早期 return しない。
    pub fn matches(&self, presented: &[u8]) -> bool {
        let expected = self.expected.as_bytes();
        let max = expected.len().max(presented.len());
        let mut acc = u8::from(expected.len() != presented.len());
        for i in 0..max {
            let a = expected.get(i).copied().unwrap_or(0);
            let b = presented.get(i).copied().unwrap_or(0);
            acc |= a ^ b;
        }
        acc == 0 && !self.expected.is_empty()
    }

    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), GateError> {
        if self.expected.is_empty() {
            return Err(GateError::new(ErrorCode::Unauthorized));
        }
        let presented = match presented_token(headers) {
            Some(t) => t,
            None => return Err(GateError::new(ErrorCode::Unauthorized)),
        };
        if self.matches(presented.as_bytes()) {
            Ok(())
        } else {
            Err(GateError::new(ErrorCode::Unauthorized))
        }
    }
}

/// exact `Bearer <token>`。scheme 違いは None（= 401）。
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn empty_expected_rejects() {
        let token = OperatorToken::from_bytes("");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        assert!(token.authorize(&headers).is_err());
    }

    #[test]
    fn compare_runs_to_max_len() {
        let token = OperatorToken::from_bytes("abcd");
        assert!(!token.matches(b"ab"));
        assert!(!token.matches(b"abcdef"));
        assert!(!token.matches(b"abcx"));
        assert!(token.matches(b"abcd"));
    }

    #[test]
    fn debug_is_redacted() {
        let token = OperatorToken::from_bytes("secret-value");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("secret-value"));
        assert_eq!(rendered, "OperatorToken(redacted)");
    }
}
