//! OpenAI TTS（/v1/audio/speech）。response_format=wav で受ける。

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::TtsProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiTtsProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl OpenAiTtsProvider {
    pub fn new(base_url: Option<String>, model: String, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model,
            api_key,
        }
    }
}

#[async_trait]
impl TtsProvider for OpenAiTtsProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn synthesize(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .post(format!("{}/audio/speech", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "model": self.model,
                "input": text,
                "voice": voice,
                "response_format": "wav",
            }))
            .send()
            .await
            .context("OpenAI TTS request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI TTS failed ({status}): {body}");
        }
        Ok(resp
            .bytes()
            .await
            .context("OpenAI TTS: failed to read audio")?
            .to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_util::spawn_http_mock;

    #[tokio::test]
    async fn test_synthesize_request_shape() {
        let (url, captured) =
            spawn_http_mock("200 OK", "audio/wav", b"RIFFxxxxWAVE".to_vec()).await;
        let p = OpenAiTtsProvider::new(Some(url), "gpt-4o-mini-tts".into(), "sk-tts".into());
        let out = p.synthesize("hello", "alloy").await.unwrap();
        assert_eq!(&out[..4], b"RIFF");
        let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(req.starts_with("POST /audio/speech"));
        assert!(req.contains("\"voice\":\"alloy\""));
        assert!(req.contains("\"response_format\":\"wav\""));
    }

    #[tokio::test]
    async fn test_error_status() {
        let (url, _) =
            spawn_http_mock("429 Too Many Requests", "application/json", b"{}".to_vec()).await;
        let p = OpenAiTtsProvider::new(Some(url), "m".into(), "k".into());
        let err = p.synthesize("x", "alloy").await.unwrap_err().to_string();
        assert!(err.contains("429"), "{err}");
    }
}
