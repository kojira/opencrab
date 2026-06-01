//! Subtask lifecycle webhook delivery.
//!
//! opencrab 本体の subtask lifecycle を authoritative な source of truth として、
//! started / completed / failed / timed_out / aborted を Discord webhook へ配送する。
//!
//! 設計: docs/subtask-webhook-tracking-design.md (Phase 1)
//!
//! - raw task text はそのまま送る（要約も redact もしない）
//! - 長文は安全な長さに chunk 化し、part X/N を付けて順次送信する
//! - 同一 run の配送は 1 本の mpsc チャネル + 1 worker で直列化し、ordering を保証する
//!   （別 run の worker とは並行に動くため interleave しうるが、同一 run 内は順序維持）
//! - 429 は Retry-After を尊重し、その他失敗は best-effort backoff retry する

use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

/// Discord メッセージの安全な本文長（2000 上限に対し metadata 用の余裕を残す）。
pub const DISCORD_CHUNK_LIMIT: usize = 1900;
const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// spawn 時に渡される webhook 設定（最小形）。
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookConfig {
    pub url: String,
    /// 送信対象イベント名。None の場合は全イベントを送る。
    pub events: Option<Vec<String>>,
}

impl WebhookConfig {
    /// spawn_subtask の引数から webhook 設定を取り出す。
    ///
    /// 期待する最小 JSON 形:
    /// ```json
    /// { "webhook": { "url": "https://...", "events": ["started", "completed"] } }
    /// ```
    /// `events` は省略可能。`url` が無い/空なら無効として None を返す。
    pub fn from_args(args: &serde_json::Value) -> Option<WebhookConfig> {
        let wh = args.get("webhook")?;
        let url = wh.get("url").and_then(|v| v.as_str())?.to_string();
        if url.is_empty() {
            return None;
        }
        let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        Some(WebhookConfig { url, events })
    }

    /// 指定イベントを送るべきか。events 未指定なら常に true。
    pub fn wants(&self, event: &str) -> bool {
        match &self.events {
            Some(list) => {
                if list.iter().any(|e| normalize_event_name(e) == event) {
                    return true;
                }
                // Backward compatibility for callers that created lifecycle streams before
                // progress existed: started/completed streams should include tool progress too.
                event == "progress" && list.iter().any(|e| normalize_event_name(e) == "started")
            }
            None => true,
        }
    }
}

fn normalize_event_name(event: &str) -> &str {
    event.strip_prefix("subtask.").unwrap_or(event)
}

/// lifecycle イベントの共通メタ情報（payload 整形用）。
#[derive(Clone, Debug)]
pub struct LifecycleMeta {
    pub label: String,
    pub run_id: String,
    pub session_key: String,
}

/// 配送 worker に渡す 1 バッチ。messages は同一 run 内で順序通りに送る。
#[derive(Clone, Debug)]
pub struct DeliveryBatch {
    pub url: String,
    pub messages: Vec<String>,
}

/// raw text を char 単位で limit 以下の chunk に分割する（UTF-8 境界を壊さない）。
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() || limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// started 用のメッセージ列を組み立てる。
///
/// 1 通目: メタ情報（label / runId / sessionKey / status / part count）
/// 続く複数通: raw task text の chunk（part X/N 付き）
pub fn build_started_messages(
    meta: &LifecycleMeta,
    raw_task_text: &str,
    limit: usize,
) -> Vec<String> {
    let chunks = chunk_text(raw_task_text, limit);
    let part_count = chunks.len();
    let mut msgs = Vec::with_capacity(part_count + 1);
    msgs.push(format!(
        "🟢 **subtask started**\nlabel: `{}`\nrunId: `{}`\nsessionKey: `{}`\nstatus: `started`\nparts: {}",
        meta.label, meta.run_id, meta.session_key, part_count
    ));
    for (i, c) in chunks.iter().enumerate() {
        msgs.push(format!("part {}/{}\n{}", i + 1, part_count, c));
    }
    msgs
}

/// completed / failed / timed_out / aborted 用の簡潔なステータスメッセージ。
///
/// detail は result summary もしくは error message。長い場合は安全長に丸める。
pub fn build_terminal_message(
    status: &str,
    run_id: &str,
    session_key: &str,
    duration_ms: Option<u64>,
    detail: &str,
) -> String {
    let emoji = match status {
        "completed" => "✅",
        "failed" => "❌",
        "timed_out" => "⏱️",
        "aborted" => "🛑",
        _ => "ℹ️",
    };
    let dur = duration_ms
        .map(|d| format!("{}ms", d))
        .unwrap_or_else(|| "-".to_string());
    let mut s = format!(
        "{} **subtask {}**\nrunId: `{}`\nsessionKey: `{}`\nduration: {}",
        emoji, status, run_id, session_key, dur
    );
    if !detail.trim().is_empty() {
        // 1 通に収まるよう丸める（chunk 化はしない: terminal は概要のみ）。
        let label = if status == "completed" {
            "result"
        } else {
            "error"
        };
        let prefix = format!("\n{}: ", label);
        let remaining =
            DISCORD_MESSAGE_LIMIT.saturating_sub(s.chars().count() + prefix.chars().count());
        let trimmed = truncate_chars(detail, remaining);
        s.push_str(&prefix);
        s.push_str(&trimmed);
    }
    s
}

/// progress 用の短いステータスメッセージ。
pub fn build_progress_message(run_id: &str, session_key: &str, message: &str) -> String {
    let mut s = format!(
        "🔄 **subtask progress**\nrunId: `{}`\nsessionKey: `{}`",
        run_id, session_key
    );
    if !message.trim().is_empty() {
        let prefix = "\nmessage: ";
        let remaining =
            DISCORD_MESSAGE_LIMIT.saturating_sub(s.chars().count() + prefix.chars().count());
        let trimmed = truncate_chars(message, remaining);
        s.push_str(prefix);
        s.push_str(&trimmed);
    }
    s
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    text.chars().take(limit).collect()
}

/// 指定 run 用の配送チャネルと worker を起動し、送信側 sender を返す。
///
/// worker は受信した DeliveryBatch を順次処理し、各メッセージを Discord webhook へ
/// 直列送信する。sender が全て drop されチャネルが閉じると worker は終了する。
pub fn spawn_run_worker(client: reqwest::Client) -> mpsc::UnboundedSender<DeliveryBatch> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DeliveryBatch>();
    tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            for msg in &batch.messages {
                send_with_retry(&client, &batch.url, msg).await;
            }
        }
    });
    tx
}

/// 1 メッセージを Discord webhook へ送る。429 は Retry-After を尊重し、
/// その他失敗は best-effort backoff retry する。
async fn send_with_retry(client: &reqwest::Client, url: &str, content: &str) {
    // 即時 / 2s / 10s / 30s / 120s
    const BACKOFFS: [u64; 5] = [0, 2, 10, 30, 120];
    let mut attempt = 0usize;
    let safe_url = redact_webhook_url(url);
    loop {
        let body = json!({ "content": content });
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 429 {
                    let retry_after = parse_retry_after(&resp).unwrap_or(1.0);
                    tracing::warn!(url = %safe_url, retry_after, "discord webhook 429, respecting Retry-After");
                    tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                    // 429 は同一メッセージを再送（attempt は進めない: ordering 維持）。
                    continue;
                }
                if status.is_success() {
                    return;
                }
                let response_text = resp.text().await.unwrap_or_default();
                let response_preview = truncate_chars(&response_text, 500);
                tracing::warn!(
                    url = %safe_url,
                    status = status.as_u16(),
                    response = %response_preview,
                    "discord webhook non-success"
                );
            }
            Err(e) => {
                tracing::warn!(url = %safe_url, error = %e, "discord webhook request error");
            }
        }
        attempt += 1;
        if attempt >= BACKOFFS.len() {
            tracing::error!(url = %safe_url, "discord webhook delivery gave up after retries");
            return;
        }
        tokio::time::sleep(Duration::from_secs(BACKOFFS[attempt])).await;
    }
}

fn redact_webhook_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/[redacted]"),
        None => "[redacted]".to_string(),
    }
}

/// Retry-After ヘッダ（秒）を読む。
fn parse_retry_after(resp: &reqwest::Response) -> Option<f64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}

/// exit_reason（subtask engine の内部値）を webhook ステータスへ写像する。
pub fn exit_reason_to_status(exit_reason: &str) -> &'static str {
    match exit_reason {
        "timeout" => "timed_out",
        "error" => "failed",
        // "completed" / "stopped_by_limit" など
        _ => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_chunk_text_empty() {
        assert!(chunk_text("", 10).is_empty());
        assert!(chunk_text("abc", 0).is_empty());
    }

    #[test]
    fn test_chunk_text_shorter_than_limit() {
        let chunks = chunk_text("hello", 10);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn test_chunk_text_splits_in_order() {
        let chunks = chunk_text("abcdefg", 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
        // reconstruction preserves order/content
        assert_eq!(chunks.concat(), "abcdefg");
    }

    #[test]
    fn test_chunk_text_respects_utf8_boundaries() {
        // multibyte chars must not be split mid-byte
        let chunks = chunk_text("あいうえお", 2);
        assert_eq!(chunks, vec!["あい", "うえ", "お"]);
        assert_eq!(chunks.concat(), "あいうえお");
    }

    #[test]
    fn test_build_started_messages_metadata_first_then_chunks() {
        let meta = LifecycleMeta {
            label: "lbl".to_string(),
            run_id: "run1".to_string(),
            session_key: "sess1".to_string(),
        };
        let msgs = build_started_messages(&meta, "abcdef", 3);
        // metadata + 2 chunks
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0].contains("subtask started"));
        assert!(msgs[0].contains("run1"));
        assert!(msgs[0].contains("sess1"));
        assert!(msgs[0].contains("parts: 2"));
        assert!(msgs[1].starts_with("part 1/2\n"));
        assert!(msgs[1].ends_with("abc"));
        assert!(msgs[2].starts_with("part 2/2\n"));
        assert!(msgs[2].ends_with("def"));
    }

    #[test]
    fn test_build_started_messages_empty_task() {
        let meta = LifecycleMeta {
            label: "lbl".to_string(),
            run_id: "run1".to_string(),
            session_key: "sess1".to_string(),
        };
        let msgs = build_started_messages(&meta, "", 100);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("parts: 0"));
    }

    #[test]
    fn test_build_started_messages_chunks_within_limit() {
        let meta = LifecycleMeta {
            label: "lbl".to_string(),
            run_id: "r".to_string(),
            session_key: "s".to_string(),
        };
        let long = "x".repeat(5000);
        let msgs = build_started_messages(&meta, &long, DISCORD_CHUNK_LIMIT);
        // each message (incl. part prefix) must stay under Discord's 2000 hard limit
        for m in &msgs {
            assert!(m.chars().count() < 2000, "message too long: {}", m.len());
        }
    }

    #[test]
    fn test_build_terminal_message_variants() {
        let m = build_terminal_message("completed", "r", "s", Some(1234), "done ok");
        assert!(m.contains("✅"));
        assert!(m.contains("completed"));
        assert!(m.contains("1234ms"));
        assert!(m.contains("result: done ok"));

        let f = build_terminal_message("failed", "r", "s", None, "boom");
        assert!(f.contains("❌"));
        assert!(f.contains("duration: -"));
        assert!(f.contains("error: boom"));

        let t = build_terminal_message("timed_out", "r", "s", Some(5), "");
        assert!(t.contains("⏱️"));
        // no detail line when empty
        assert!(!t.contains("error:"));

        let a = build_terminal_message("aborted", "r", "s", Some(5), "cancelled");
        assert!(a.contains("🛑"));
        assert!(a.contains("aborted"));
    }

    #[test]
    fn test_build_terminal_message_truncates_long_detail() {
        let long = "y".repeat(5000);
        let m = build_terminal_message("failed", "r", "s", Some(1), &long);
        assert!(m.chars().count() <= DISCORD_MESSAGE_LIMIT);
    }

    #[test]
    fn test_build_progress_message_truncates_long_message() {
        let long = "x".repeat(10_000);
        let m = build_progress_message("r", "s", &long);
        assert!(m.contains("subtask progress"));
        assert!(m.chars().count() <= DISCORD_MESSAGE_LIMIT);
    }

    #[test]
    fn test_webhook_config_from_args() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://discord.com/api/webhooks/x", "events": ["started", "completed"] }
        }))
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(
            cfg.events,
            Some(vec!["started".to_string(), "completed".to_string()])
        );
    }

    #[test]
    fn test_webhook_config_from_args_no_events() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://x" }
        }))
        .unwrap();
        assert_eq!(cfg.events, None);
    }

    #[test]
    fn test_webhook_config_from_args_missing_or_empty() {
        assert!(WebhookConfig::from_args(&json!({})).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": {} })).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "" } })).is_none());
    }

    #[test]
    fn test_webhook_config_wants() {
        let all = WebhookConfig {
            url: "u".to_string(),
            events: None,
        };
        assert!(all.wants("started"));
        assert!(all.wants("progress"));
        assert!(all.wants("aborted"));

        let filtered = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["completed".to_string()]),
        };
        assert!(filtered.wants("completed"));
        assert!(!filtered.wants("started"));
        assert!(!filtered.wants("progress"));

        let lifecycle = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["started".to_string(), "completed".to_string()]),
        };
        assert!(lifecycle.wants("progress"));

        let fully_qualified = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["subtask.started".to_string()]),
        };
        assert!(fully_qualified.wants("started"));
    }

    #[test]
    fn test_exit_reason_to_status_mapping() {
        assert_eq!(exit_reason_to_status("completed"), "completed");
        assert_eq!(exit_reason_to_status("stopped_by_limit"), "completed");
        assert_eq!(exit_reason_to_status("error"), "failed");
        assert_eq!(exit_reason_to_status("timeout"), "timed_out");
    }
}
