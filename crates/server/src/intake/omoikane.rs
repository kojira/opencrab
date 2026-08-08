//! omoikane（ナレッジベース）source アダプタ — webhook intake の第一号（issue #454）。
//!
//! catch-up の真実は omoikane 側の一覧 API。`GET {base}/v1/comments/recent` を Bearer で
//! 叩き、最近のコメントを [`IntakeEvent`] に変換して返す。webhook 配送（`comment.created`）
//! と **同じ dedup_key**（`comment.created:{id}`）を作るので、同一コメントが webhook と
//! catch-up の両方から来ても二重に積まれない。
//!
//! 秘密（Bearer トークン）はこの構造体に閉じ込め、ログ・エラーには出さない。

use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::OmoikaneConfig;

#[cfg(test)]
use super::webhook_dedup_key;
use super::{json_scalar_id, IntakeEvent, SourceAdapter};

/// omoikane source 名（webhook のパス `/api/hooks/omoikane` と対）。
pub const SOURCE: &str = "omoikane";

/// 最近のコメント一覧が生む event_type。webhook 側の `type` と一致させること。
pub const EVENT_COMMENT_CREATED: &str = "comment.created";

const CONNECT_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

pub struct OmoikaneAdapter {
    client: reqwest::Client,
    base_url: String,
    bearer_token: String,
    entry_created_by: String,
    poll_limit: u32,
}

impl OmoikaneAdapter {
    /// 設定からアダプタを組む。`enabled=false` / `base_url` 空なら `None`（catch-up しない）。
    pub fn from_config(cfg: &OmoikaneConfig) -> Option<Self> {
        if !cfg.enabled || cfg.base_url.trim().is_empty() {
            return None;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();
        Some(Self {
            client,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            bearer_token: cfg.bearer_token.clone(),
            entry_created_by: cfg.entry_created_by.clone(),
            poll_limit: cfg.poll_limit,
        })
    }

    fn recent_comments_url(&self) -> String {
        format!(
            "{}/v1/comments/recent?entry_created_by={}&limit={}",
            self.base_url,
            urlencode(&self.entry_created_by),
            self.poll_limit
        )
    }
}

#[async_trait::async_trait]
impl SourceAdapter for OmoikaneAdapter {
    fn source(&self) -> &str {
        SOURCE
    }

    async fn poll_recent(&self) -> Result<Vec<IntakeEvent>> {
        let url = self.recent_comments_url();
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.bearer_token))
            .header("Accept", "application/json")
            .send()
            .await
            .context("omoikane: recent comments request failed")?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("omoikane: failed to read response body")?;
        if !status.is_success() {
            // 本文にトークンは含まれない想定だが、念のため短く切って外へ出す情報を絞る。
            anyhow::bail!(
                "omoikane: recent comments returned {} ({} bytes)",
                status,
                text.len()
            );
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).context("omoikane: response is not valid JSON")?;
        let items = extract_comment_array(&value);
        let mut events = Vec::with_capacity(items.len());
        for item in items {
            // id が取れないコメントは dedup できないので飛ばす（payload hash に落とすと
            // catch-up の度に別キーになり、毎回積み直してしまうため）。
            let Some(id) = json_scalar_id(item.get("id")) else {
                continue;
            };
            events.push(IntakeEvent {
                event_type: EVENT_COMMENT_CREATED.to_string(),
                dedup_key: format!("{EVENT_COMMENT_CREATED}:{id}"),
                payload_json: item.to_string(),
            });
        }
        Ok(events)
    }
}

/// レスポンスからコメント配列を取り出す。トップレベル配列 / `comments` / `data` / `items`
/// のいずれかを許容する（omoikane#33 の正確な形は 要ビルド検証。ここは防御的に読む）。
fn extract_comment_array(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    for key in ["comments", "data", "items", "results"] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

/// 最小の URL クエリエンコード（英数と `-_.~` 以外を %XX に）。uid は限定文字だが安全側で。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// webhook_dedup_key は catch-up と同じキー規則を共有していることの回帰ガード。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catchup_and_webhook_agree_on_dedup_key() {
        let data = serde_json::json!({"id": 42, "text": "hi"});
        // webhook 側（raw body の data から）
        let wh = webhook_dedup_key(EVENT_COMMENT_CREATED, &data, b"{}");
        // catch-up 側（アダプタが作る形）
        let cu = format!("{EVENT_COMMENT_CREATED}:42");
        assert_eq!(
            wh, cu,
            "webhook と catch-up の dedup_key が食い違うと二重に積む"
        );
    }

    #[test]
    fn extract_array_shapes() {
        let bare = serde_json::json!([{"id": 1}]);
        assert_eq!(extract_comment_array(&bare).len(), 1);
        let wrapped = serde_json::json!({"comments": [{"id": 1}, {"id": 2}]});
        assert_eq!(extract_comment_array(&wrapped).len(), 2);
        let none = serde_json::json!({"x": 1});
        assert!(extract_comment_array(&none).is_empty());
    }

    #[test]
    fn disabled_or_empty_base_yields_no_adapter() {
        let mut cfg = OmoikaneConfig {
            enabled: false,
            base_url: "https://x".into(),
            bearer_token: String::new(),
            entry_created_by: String::new(),
            poll_limit: 50,
        };
        assert!(OmoikaneAdapter::from_config(&cfg).is_none());
        cfg.enabled = true;
        cfg.base_url = "  ".into();
        assert!(OmoikaneAdapter::from_config(&cfg).is_none());
        cfg.base_url = "https://x/".into();
        let a = OmoikaneAdapter::from_config(&cfg).unwrap();
        assert_eq!(a.base_url, "https://x"); // 末尾スラッシュを剥がす
    }
}
