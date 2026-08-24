//! VOICEVOX エンジン TTS。
//!
//! ローカルの VOICEVOX ENGINE（既定 http://localhost:50021）を使う。
//! 2 段階 API: POST /audio_query（クエリ生成）→ POST /synthesis（WAV 生成）。
//! `voice` はスタイル ID の数字文字列（例: "3" = ずんだもん ノーマル）。
//! エージェントごとに別のスタイル ID を割り当てれば声を聴き分けられる。

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::TtsProvider;

const DEFAULT_BASE_URL: &str = "http://localhost:50021";

pub struct VoicevoxProvider {
    client: reqwest::Client,
    base_url: String,
}

impl VoicevoxProvider {
    pub fn new(base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        }
    }
}

#[async_trait]
impl TtsProvider for VoicevoxProvider {
    fn name(&self) -> &str {
        "voicevox"
    }

    async fn synthesize(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        let speaker: u32 = voice.trim().parse().with_context(|| {
            format!("VOICEVOX の話者はスタイルIDの数字で指定してください: {voice:?}")
        })?;

        // 1. audio_query（テキストはクエリパラメータで渡す仕様）
        let query_resp = self
            .client
            .post(format!("{}/audio_query", self.base_url))
            .query(&[("text", text), ("speaker", &speaker.to_string())])
            .send()
            .await
            .context(
                "VOICEVOX audio_query request failed — VOICEVOX ENGINE は起動していますか？",
            )?;
        let status = query_resp.status();
        let query_body = query_resp
            .text()
            .await
            .context("VOICEVOX audio_query: failed to read body")?;
        if !status.is_success() {
            anyhow::bail!("VOICEVOX audio_query failed ({status}): {query_body}");
        }

        // 2. synthesis
        let synth_resp = self
            .client
            .post(format!("{}/synthesis", self.base_url))
            .query(&[("speaker", speaker.to_string())])
            .header("content-type", "application/json")
            .body(query_body)
            .send()
            .await
            .context("VOICEVOX synthesis request failed")?;
        let status = synth_resp.status();
        if !status.is_success() {
            let body = synth_resp.text().await.unwrap_or_default();
            anyhow::bail!("VOICEVOX synthesis failed ({status}): {body}");
        }
        let wav = synth_resp
            .bytes()
            .await
            .context("VOICEVOX synthesis: failed to read audio")?;
        Ok(wav.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_util::spawn_http_mock;

    #[tokio::test]
    async fn test_synthesize_two_step() {
        // audio_query と synthesis を同じモックで受ける（両方 POST）。
        // 1 回目は JSON クエリ、2 回目は WAV を返すため、レスポンスは
        // 両対応の JSON でも synthesis 側はバイト列として受けるので問題ない。
        let (url, captured) = spawn_http_mock(
            "200 OK",
            "application/json",
            br#"{"accent_phrases":[],"speedScale":1.0}"#.to_vec(),
        )
        .await;
        let p = VoicevoxProvider::new(Some(url));
        let out = p.synthesize("こんにちは", "3").await.unwrap();
        assert!(!out.is_empty());
        let req = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        // 最後のリクエスト（synthesis）が speaker=3 付き POST であること
        assert!(
            req.contains("POST /synthesis?speaker=3"),
            "{}",
            &req[..80.min(req.len())]
        );
    }

    #[tokio::test]
    async fn test_non_numeric_voice_rejected() {
        let p = VoicevoxProvider::new(Some("http://127.0.0.1:1".into()));
        let err = p.synthesize("test", "alloy").await.unwrap_err().to_string();
        assert!(err.contains("スタイルID"), "{err}");
    }
}
