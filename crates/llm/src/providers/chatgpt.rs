use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MODEL: &str = "gpt-5.5";

mod auth;
mod image;
mod request;
mod response;
mod transport;

#[cfg(test)]
use auth::{base64url_decode, token_expired};
use auth::{build_client, expand_tilde, extract_account_id, DEFAULT_TIMEOUT_SECS, OAUTH_TOKEN_URL};
#[cfg(test)]
use image::{base64_encode, guess_image_mime, is_global_ip, validate_public_url};

#[derive(Debug, Clone)]
pub struct ChatGptProvider {
    client: Client,
    /// Path to auth.json file (default: ~/.codex/auth.json)
    auth_file: String,
    base_url: String,
    /// OAuth トークンリフレッシュ先（テストで差し替え可能）。
    oauth_token_url: String,
    default_model: String,
    reasoning_effort: Option<String>,
    include_encrypted_content: bool,
    /// `client` に設定済みの read timeout（秒）。`Client` からは読み出せないので保持する。
    timeout_secs: u64,
    /// テレメトリ用の表示名（既定は形式名 "chatgpt"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl Default for ChatGptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptProvider {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            client: build_client(DEFAULT_TIMEOUT_SECS),
            auth_file: format!("{}/.codex/auth.json", home),
            base_url: CHATGPT_BASE_URL.to_string(),
            oauth_token_url: OAUTH_TOKEN_URL.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            reasoning_effort: Some("low".to_string()),
            include_encrypted_content: false,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            name: "chatgpt".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// チャット補完リクエストの read timeout を秒で上書きする（#433）。
    ///
    /// 既定は 60 秒。`reasoning_effort` の高い体は 1 ターンの生成がこれを超えることが
    /// あり、超えると `failed to read response body: operation timed out` になって
    /// router がリトライする。config の `[providers.chatgpt] timeout_secs` から渡す。
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.client = build_client(secs);
        self.timeout_secs = secs;
        self
    }

    /// テスト用: OAuth トークンエンドポイントを差し替える。
    pub fn with_oauth_token_url(mut self, url: impl Into<String>) -> Self {
        self.oauth_token_url = url.into();
        self
    }

    pub fn with_auth_file(mut self, path: impl Into<String>) -> Self {
        let p: String = path.into();
        self.auth_file = expand_tilde(&p);
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let s: String = effort.into();
        self.reasoning_effort = if s.is_empty() { None } else { Some(s) };
        self
    }

    pub fn with_include_encrypted_content(mut self, v: bool) -> Self {
        self.include_encrypted_content = v;
        self
    }
}

#[cfg(test)]
#[path = "chatgpt/tests/mod.rs"]
mod tests;
