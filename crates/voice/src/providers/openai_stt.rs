//! OpenAI 互換 STT（/v1/audio/transcriptions, multipart）。
//!
//! base_url を差し替えれば faster-whisper-server / LocalAI 等の
//! Whisper 互換ローカルサーバでもそのまま動く。

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::SttProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiSttProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl OpenAiSttProvider {
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
impl SttProvider for OpenAiSttProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn transcribe(&self, wav: &[u8], language: Option<&str>) -> Result<String> {
        let file_part = reqwest::multipart::Part::bytes(wav.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .context("invalid mime")?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", self.model.clone())
            .text("response_format", "json");
        if let Some(lang) = language {
            form = form.text("language", lang.to_string());
        }

        let mut req = self
            .client
            .post(format!("{}/audio/transcriptions", self.base_url))
            .multipart(form);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.context("STT request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("STT: failed to read body")?;
        if !status.is_success() {
            anyhow::bail!("STT failed ({status}): {text}");
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("STT: invalid JSON response")?;
        Ok(parsed["text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_util::spawn_http_mock;

    #[tokio::test]
    async fn test_transcribe_sends_multipart_and_parses_text() {
        let (url, captured) = spawn_http_mock(
            "200 OK",
            "application/json",
            r#"{"text":" こんにちは "}"#.as_bytes().to_vec(),
        )
        .await;
        let p = OpenAiSttProvider::new(Some(url), "whisper-1".into(), "sk-test".into());
        let wav = crate::audio::pcm_to_wav(&[0i16; 1600], 16000, 1);
        let out = p.transcribe(&wav, Some("ja")).await.unwrap();
        assert_eq!(out, "こんにちは");

        let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(
            req.starts_with("POST /audio/transcriptions"),
            "{}",
            &req[..60.min(req.len())]
        );
        assert!(
            req.contains("Bearer sk-test") || req.contains("bearer sk-test"),
            "auth header missing"
        );
        assert!(req.contains("whisper-1"));
        assert!(req.contains("name=\"language\"") && req.contains("ja"));
        assert!(req.contains("audio.wav"));
    }

    #[tokio::test]
    async fn test_transcribe_error_status() {
        let (url, _) =
            spawn_http_mock("401 Unauthorized", "application/json", b"{}".to_vec()).await;
        let p = OpenAiSttProvider::new(Some(url), "whisper-1".into(), "".into());
        let err = p
            .transcribe(&[0u8; 44], None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
    }
}
