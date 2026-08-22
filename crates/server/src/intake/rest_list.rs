//! REST 一覧アダプタ — catch-up の汎用実装（issue #470）。
//!
//! `kind = "rest_list"` の source は、`base_url` + `list_path` + `query` から GET URL を組み、
//! 返ってきた配列の各要素を [`IntakeEvent`] に写す。`id_field` の値で dedup_key
//! `{event_type}:{id}` を作るので、webhook 配送と**同じキー**になり相互に重複を弾く。
//! **特定サービス名に依存しない**——`[[intake.sources]]` を書くだけで 2 つ目・3 つ目の source を
//! コード変更なしで足せる（#470 の目的）。第一号 sample-source も config の値としてこの型で構成する。
//!
//! 秘密（Bearer トークン）はこの構造体に閉じ込め、ログ・エラーには出さない。

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{IntakeAuth, IntakeSourceConfig};

#[cfg(test)]
use super::webhook_dedup_key;
use super::{json_scalar_id, IntakeEvent, SourceAdapter};

const CONNECT_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

pub struct RestListAdapter {
    client: reqwest::Client,
    source: String,
    /// 構築時に固定した GET URL（base + path + query）。query は静的設定なので不変。
    url: String,
    /// Bearer トークン（設定時のみ）。秘密。ヘッダ以外には出さない。
    bearer_token: Option<String>,
    id_field: String,
    event_type: String,
    array_path: Option<String>,
}

impl RestListAdapter {
    /// 設定からアダプタを組む。`enabled=false` / `name` 空 / `base_url` 空 / `event_type` 空なら
    /// `None`（catch-up しない）。event_type が無いと dedup_key もルーティングも作れない。
    pub fn from_config(cfg: &IntakeSourceConfig) -> Option<Self> {
        if !cfg.enabled
            || cfg.name.trim().is_empty()
            || cfg.base_url.trim().is_empty()
            || cfg.event_type.trim().is_empty()
        {
            return None;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();
        // 空トークンの Bearer は付けない（`Authorization: Bearer ` を送らない）。
        let bearer_token = match &cfg.auth {
            IntakeAuth::None => None,
            IntakeAuth::Bearer { token } if token.is_empty() => None,
            IntakeAuth::Bearer { token } => Some(token.clone()),
        };
        Some(Self {
            client,
            source: cfg.name.clone(),
            url: build_url(&cfg.base_url, &cfg.list_path, &cfg.query),
            bearer_token,
            id_field: cfg.id_field.clone(),
            event_type: cfg.event_type.clone(),
            array_path: cfg.array_path.clone(),
        })
    }
}

#[async_trait::async_trait]
impl SourceAdapter for RestListAdapter {
    fn source(&self) -> &str {
        &self.source
    }

    async fn poll_recent(&self) -> Result<Vec<IntakeEvent>> {
        // reqwest のエラー Display は URL を載せる。URL のクエリに識別子（uid 等）が入りうるため、
        // 公開ログに出さないよう `without_url()` で剥がしてから文脈を付ける。
        let mut req = self
            .client
            .get(&self.url)
            .header("Accept", "application/json");
        if let Some(token) = &self.bearer_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("intake rest_list: request failed")?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| e.without_url())
            .context("intake rest_list: failed to read response body")?;
        if !status.is_success() {
            // 本文にトークンは含まれない想定だが、外へ出す情報を絞る（長さのみ）。
            anyhow::bail!(
                "intake rest_list: list API returned {} ({} bytes)",
                status,
                text.len()
            );
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).context("intake rest_list: response is not valid JSON")?;
        let items = extract_array(&value, self.array_path.as_deref());
        let mut events = Vec::with_capacity(items.len());
        for item in &items {
            // id が取れない要素は dedup できないので飛ばす（現行 sample-source と同一挙動を保持）。
            if let Some(ev) = build_event(&self.event_type, &self.id_field, item) {
                events.push(ev);
            }
        }
        Ok(events)
    }
}

/// 1 要素を [`IntakeEvent`] に写す。`id_field` の値が取れなければ `None`（＝捨てる）。
/// dedup_key は webhook と同じ `{event_type}:{id}` 規則（相互 dedup を保つ）。
fn build_event(event_type: &str, id_field: &str, item: &serde_json::Value) -> Option<IntakeEvent> {
    let id = json_scalar_id(item.get(id_field))?;
    Some(IntakeEvent {
        event_type: event_type.to_string(),
        dedup_key: format!("{event_type}:{id}"),
        payload_json: item.to_string(),
    })
}

/// `base_url` + `list_path` + `query` から GET URL を組む。末尾/先頭スラッシュを整え、query は
/// キー順（`BTreeMap` で決定的）に URL エンコードして付ける。query が空なら `?` を付けない。
fn build_url(base_url: &str, list_path: &str, query: &BTreeMap<String, String>) -> String {
    let mut url = String::from(base_url.trim_end_matches('/'));
    if !list_path.is_empty() {
        if !list_path.starts_with('/') {
            url.push('/');
        }
        url.push_str(list_path);
    }
    if !query.is_empty() {
        url.push('?');
        for (i, (k, v)) in query.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str(&urlencode(k));
            url.push('=');
            url.push_str(&urlencode(v));
        }
    }
    url
}

/// レスポンスから配列を取り出す。`array_path` 指定時は**そのトップレベルキーのみ**見る
/// （明示が勝つ・省略検出にフォールバックしない）。省略時は防御的に自動検出
/// （トップレベル配列 / `comments` / `data` / `items` / `results`）。
fn extract_array(value: &serde_json::Value, array_path: Option<&str>) -> Vec<serde_json::Value> {
    if let Some(key) = array_path {
        return value
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
    }
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

/// 最小の URL クエリエンコード（英数と `-_.~` 以外を %XX に）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IntakeAuth, IntakeSourceConfig, IntakeSourceKind};

    fn base_cfg() -> IntakeSourceConfig {
        IntakeSourceConfig {
            name: "sample-source".into(),
            kind: IntakeSourceKind::RestList,
            enabled: true,
            base_url: "https://kb.example/".into(),
            auth: IntakeAuth::Bearer {
                token: "secret-token".into(),
            },
            list_path: "/v1/comments/recent".into(),
            query: BTreeMap::from([
                ("entry_created_by".into(), "uid-123".into()),
                ("limit".into(), "50".into()),
            ]),
            id_field: "id".into(),
            event_type: "comment.created".into(),
            array_path: None,
        }
    }

    #[test]
    fn build_url_joins_and_sorts_query() {
        let cfg = base_cfg();
        let url = build_url(&cfg.base_url, &cfg.list_path, &cfg.query);
        // 末尾スラッシュを剥がし、query はキー順（entry_created_by < limit）で決定的。
        assert_eq!(
            url,
            "https://kb.example/v1/comments/recent?entry_created_by=uid-123&limit=50"
        );
    }

    #[test]
    fn build_url_handles_missing_slash_and_empty_query() {
        let q = BTreeMap::new();
        assert_eq!(
            build_url("https://x", "v1/list", &q),
            "https://x/v1/list",
            "list_path の先頭スラッシュを補い、query 空なら ? を付けない"
        );
    }

    #[test]
    fn build_url_percent_encodes() {
        let q = BTreeMap::from([("q".into(), "a b/c".into())]);
        assert_eq!(build_url("https://x", "/s", &q), "https://x/s?q=a%20b%2Fc");
    }

    #[test]
    fn from_config_builds_with_expected_source_and_url() {
        let a = RestListAdapter::from_config(&base_cfg()).expect("valid cfg builds");
        assert_eq!(a.source(), "sample-source");
        assert_eq!(
            a.url,
            "https://kb.example/v1/comments/recent?entry_created_by=uid-123&limit=50"
        );
        assert_eq!(a.bearer_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn from_config_gates_on_disabled_empty_base_or_event_type() {
        let mut cfg = base_cfg();
        cfg.enabled = false;
        assert!(RestListAdapter::from_config(&cfg).is_none(), "disabled");
        cfg = base_cfg();
        cfg.base_url = "   ".into();
        assert!(
            RestListAdapter::from_config(&cfg).is_none(),
            "empty base_url"
        );
        cfg = base_cfg();
        cfg.event_type = String::new();
        assert!(
            RestListAdapter::from_config(&cfg).is_none(),
            "empty event_type"
        );
        cfg = base_cfg();
        cfg.name = String::new();
        assert!(RestListAdapter::from_config(&cfg).is_none(), "empty name");
    }

    #[test]
    fn from_config_no_auth_and_empty_token_send_no_bearer() {
        let mut cfg = base_cfg();
        cfg.auth = IntakeAuth::None;
        assert!(RestListAdapter::from_config(&cfg)
            .unwrap()
            .bearer_token
            .is_none());
        cfg.auth = IntakeAuth::Bearer {
            token: String::new(),
        };
        assert!(RestListAdapter::from_config(&cfg)
            .unwrap()
            .bearer_token
            .is_none());
    }

    #[test]
    fn build_event_uses_id_field_and_matches_webhook_key() {
        let item = serde_json::json!({"id": 42, "text": "hi"});
        let ev = build_event("comment.created", "id", &item).unwrap();
        // catch-up と webhook が同じ dedup_key を作る（二重積み防止の要）。
        let wh = webhook_dedup_key("comment.created", &item, b"{}");
        assert_eq!(ev.dedup_key, wh);
        assert_eq!(ev.dedup_key, "comment.created:42");
    }

    #[test]
    fn build_event_honors_custom_id_field_and_drops_missing_id() {
        let item = serde_json::json!({"uuid": "abc", "id_ignored": 1});
        let ev = build_event("chat.message", "uuid", &item).unwrap();
        assert_eq!(ev.dedup_key, "chat.message:abc");
        // id_field が取れない要素は捨てる（None）。
        let no_id = serde_json::json!({"text": "no id here"});
        assert!(build_event("chat.message", "uuid", &no_id).is_none());
    }

    #[test]
    fn extract_array_explicit_path_only() {
        let v = serde_json::json!({"comments": [{"id": 1}], "data": [{"id": 9}]});
        // array_path 指定時はそのキーのみ（data は無視）。
        assert_eq!(extract_array(&v, Some("comments")).len(), 1);
        assert_eq!(extract_array(&v, Some("comments"))[0]["id"], 1);
        // 指定キーが配列でない/無ければ空（フォールバックしない）。
        assert!(extract_array(&v, Some("missing")).is_empty());
    }

    #[test]
    fn extract_array_autodetect_when_no_path() {
        let bare = serde_json::json!([{"id": 1}]);
        assert_eq!(extract_array(&bare, None).len(), 1);
        let wrapped = serde_json::json!({"items": [{"id": 1}, {"id": 2}]});
        assert_eq!(extract_array(&wrapped, None).len(), 2);
        let none = serde_json::json!({"x": 1});
        assert!(extract_array(&none, None).is_empty());
    }
}
