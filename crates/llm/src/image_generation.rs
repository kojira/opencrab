//! 画像生成の注入口（D-1076）。
//!
//! ツール層（`opencrab-actions` の `generate_image`）はこの [`ImageGenerator`] だけを見る。
//! 実装は ChatGPT サブスクリプション（ChatGPT OAuth）の Images エンドポイントを叩く
//! `ChatGptProvider` のみで、API キーや他プロバイダへのフォールバックは持たない。
//! 1 回の生成が利用枠を消費するため、router のリトライは通さない（失敗はそのまま返す）。

use async_trait::async_trait;

/// 画像生成の依頼。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    /// 背景を透過にする（既定は不透明）。
    pub transparent_background: bool,
}

/// 生成された画像（デコード済みの PNG バイト列）。
#[derive(Clone, PartialEq)]
pub struct GeneratedImage {
    pub png_bytes: Vec<u8>,
    /// サーバが書き換えたプロンプト（返ってきたときだけ）。
    pub revised_prompt: Option<String>,
    /// サーバが返した利用量（形はサーバ依存なのでそのまま持つ）。
    pub usage: Option<serde_json::Value>,
    /// 診断用のリクエスト ID（`x-codex-imagegen-request-id`）。
    pub request_id: Option<String>,
}

/// 画像本体をログへ出さないため、Debug はバイト数だけを表示する。
impl std::fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedImage")
            .field("png_bytes_len", &self.png_bytes.len())
            .field("revised_prompt", &self.revised_prompt)
            .field("usage", &self.usage)
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// 画像生成の失敗。どれもリトライしない（再試行は利用枠をもう一度消費する）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageGenerationError {
    /// 利用上限（HTTP 429 / `usage_limit_reached`）。`resets_at` は Unix 秒。
    #[error("image generation usage limit reached{}: {message}", resets_suffix(.resets_at))]
    UsageLimit {
        message: String,
        resets_at: Option<i64>,
    },
    /// その他の 4xx（モデレーションによる拒否など）。サーバのメッセージを載せる。
    #[error("image generation rejected (HTTP {status}): {message}")]
    Rejected { status: u16, message: String },
    /// 5xx などの 4xx 以外の失敗。
    #[error("image generation failed (HTTP {status}): {message}")]
    Server { status: u16, message: String },
    /// 成功応答だが画像データが無い。
    #[error("image generation returned no image data")]
    EmptyData,
    /// HTTP クライアントのタイムアウト。
    #[error("image generation timed out after {0}s")]
    Timeout(u64),
    /// 認証・通信・応答形式などその他の失敗。
    #[error("image generation failed: {0}")]
    Other(String),
}

fn resets_suffix(resets_at: &Option<i64>) -> String {
    match resets_at {
        Some(ts) => match chrono::DateTime::from_timestamp(*ts, 0) {
            Some(at) => format!(" (resets at {})", at.to_rfc3339()),
            None => format!(" (resets_at={ts})"),
        },
        None => String::new(),
    }
}

/// 画像を 1 枚生成する能力。server が設定済みの chatgpt プロバイダから注入する。
#[async_trait]
pub trait ImageGenerator: Send + Sync {
    async fn generate_image(
        &self,
        request: &ImageGenerationRequest,
    ) -> Result<GeneratedImage, ImageGenerationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_limit_display_includes_reset_time() {
        let err = ImageGenerationError::UsageLimit {
            message: "limit".into(),
            resets_at: Some(0),
        };
        assert_eq!(
            err.to_string(),
            "image generation usage limit reached (resets at 1970-01-01T00:00:00+00:00): limit"
        );
    }

    #[test]
    fn generated_image_debug_omits_bytes() {
        let img = GeneratedImage {
            png_bytes: vec![7; 64],
            revised_prompt: None,
            usage: None,
            request_id: None,
        };
        let dbg = format!("{img:?}");
        assert!(dbg.contains("png_bytes_len: 64"), "{dbg}");
        assert!(!dbg.contains("7, 7"), "{dbg}");
    }
}
