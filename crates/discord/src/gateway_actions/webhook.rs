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

// 通知先（webhook）の設定型・解決・URL 検証・秘匿処理・テキスト分割は gateway 非依存層
// （`opencrab_actions::webhook_target`）へ移設済み（#157 S4）。この module に残るのは
// **実際の HTTP 配送（transport）と Discord 固有の整形**だけ。既存の呼び出し元が
// `webhook::...` のまま参照できるよう、Discord 側で使う項目はここで再エクスポートする
// （crate 内部向け: `mod webhook` は private なので Discord crate の公開 API には出ない）。
// 汎用の秘匿ユーティリティ `redact_secrets` は Discord 側に利用者が居ないため re-export せず、
// 必要なら `opencrab_actions::webhook_target::redact_secrets` を直接参照する。
pub use opencrab_actions::webhook_target::{
    build_part_messages, chunk_text, has_activity_default, record_webhook_delivery_failure,
    redact_webhook_url, resolve_activity_webhook, resolve_subtask_webhook, validate_webhook_url,
    WebhookConfig, WebhookResolution, WebhookSource,
};

/// Discord メッセージの安全な本文長（2000 上限に対し metadata 用の余裕を残す）。
pub const DISCORD_CHUNK_LIMIT: usize = 1900;
const DISCORD_MESSAGE_LIMIT: usize = 2000;

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

/// started 用のメッセージ列を組み立てる。
///
/// 1 通目: メタ情報（label / runId / sessionKey / status / part count）
/// 続く複数通: raw task text の chunk（part X/N 付き）
pub fn build_started_messages(
    meta: &LifecycleMeta,
    raw_task_text: &str,
    limit: usize,
) -> Vec<String> {
    let parts = build_part_messages(raw_task_text, limit);
    let mut msgs = Vec::with_capacity(parts.len() + 1);
    msgs.push(format!(
        "🟢 **subtask started**\nlabel: `{}`\nrunId: `{}`\nsessionKey: `{}`\nstatus: `started`\nparts: {}",
        meta.label, meta.run_id, meta.session_key, parts.len()
    ));
    msgs.extend(parts);
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
#[allow(dead_code)] // 後方互換のため公開 API として維持（spawn_run_worker_with_sink へ委譲）。
pub fn spawn_run_worker(client: reqwest::Client) -> mpsc::UnboundedSender<DeliveryBatch> {
    spawn_run_worker_with_sink(client, None)
}

/// 配送失敗時に give-up を通知するための sink を受け取れる版。
///
/// `on_giveup` は送信を最終的にあきらめたとき、短いエラー説明文字列で呼ばれる
/// （raw url は渡さない）。`spawn_run_worker` は None を渡して従来挙動を保つ。
#[allow(clippy::type_complexity)]
pub fn spawn_run_worker_with_sink(
    client: reqwest::Client,
    on_giveup: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
) -> mpsc::UnboundedSender<DeliveryBatch> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DeliveryBatch>();
    tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            for msg in &batch.messages {
                send_with_retry(&client, &batch.url, msg, on_giveup.as_ref()).await;
            }
        }
    });
    tx
}

/// 1 メッセージを Discord webhook へ送る。429 は Retry-After を尊重し、
/// その他失敗は best-effort backoff retry する。
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    content: &str,
    on_giveup: Option<&std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
) {
    // 即時 / 2s / 10s / 30s / 120s
    const BACKOFFS: [u64; 5] = [0, 2, 10, 30, 120];
    let mut attempt = 0usize;
    // covered 経路（配送 debug/log）では webhook URL をマスクしない。URL がマスクされると
    // 配送先の特定・障害切り分けが困難になりデバッグ性を損なうため、生 URL をそのまま記録する
    // （docs/design-webhook-output-lossless.md §2 P4: 漏洩時は webhook を無効化して回復する）。
    let mut last_error;
    loop {
        let body = json!({ "content": content });
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 429 {
                    let retry_after = parse_retry_after(&resp).unwrap_or(1.0);
                    tracing::warn!(url = %url, retry_after, "discord webhook 429, respecting Retry-After");
                    tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                    // 429 は同一メッセージを再送（attempt は進めない: ordering 維持）。
                    continue;
                }
                if status.is_success() {
                    return;
                }
                let response_text = resp.text().await.unwrap_or_default();
                let response_preview = truncate_chars(&response_text, 500);
                last_error = format!("http {}", status.as_u16());
                tracing::warn!(
                    url = %url,
                    status = status.as_u16(),
                    response = %response_preview,
                    "discord webhook non-success"
                );
            }
            Err(e) => {
                last_error = "request error".to_string();
                tracing::warn!(url = %url, error = %e, "discord webhook request error");
            }
        }
        attempt += 1;
        if attempt >= BACKOFFS.len() {
            tracing::error!(url = %url, "discord webhook delivery gave up after retries");
            if let Some(sink) = on_giveup {
                sink(&last_error);
            }
            return;
        }
        tokio::time::sleep(Duration::from_secs(BACKOFFS[attempt])).await;
    }
}

// ---- Shell result summary (Phase 1) ----

/// execute_shell の result data から抽出した出力。
#[derive(Clone, Debug, Default)]
pub struct ShellResultSummary {
    pub exit_code: Option<i64>,
    pub stdout_summary: Option<String>,
    pub stderr_summary: Option<String>,
    pub truncated: bool,
}

/// execute_shell の ActionResult.data から exit_code / stdout / stderr / truncated を取り出す。
///
/// covered 経路（work-channel 出力）のため、redaction も head/tail クランプも一切行わず
/// stdout/stderr を full のまま返す（docs/design-webhook-output-lossless.md §2 P4）。
/// Discord のサイズ上限は `build_tool_event_message` がロスレス chunk で吸収する。
/// `truncated`（上流 execute_shell が webhook 層より前に切り捨てた = L0）はそのまま伝える。
pub fn summarize_shell_result(data: &serde_json::Value) -> ShellResultSummary {
    let exit_code = data.get("exit_code").and_then(|v| v.as_i64());
    let truncated = data
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let field = |key: &str| -> Option<String> {
        let raw = data.get(key).and_then(|v| v.as_str())?;
        if raw.is_empty() {
            return None;
        }
        Some(raw.to_string())
    };
    ShellResultSummary {
        exit_code,
        stdout_summary: field("stdout"),
        stderr_summary: field("stderr"),
        truncated,
    }
}

// ---- Tool event Discord formatting (Phase 1) ----

/// ツールイベント 1 件の整形入力（payload 概念スキーマ 3.3 の Discord 整形版）。
#[derive(Clone, Debug, Default)]
pub struct ToolEventView {
    pub event: String, // tool_call_started/completed/failed/rejected
    pub tool_name: String,
    pub tool_call_id: String,
    pub depth: u32,
    pub status: String, // started/completed/failed/rejected
    pub duration_ms: Option<u64>,
    pub args_summary: Option<String>,
    pub result_summary: Option<String>,
    pub exit_code: Option<i64>,
    pub stdout_summary: Option<String>,
    pub stderr_summary: Option<String>,
    pub truncated: bool,
    pub rejection_reason: Option<String>,
    pub max_chars: usize, // 0 → default 1500
}

/// ツールイベントを Discord 用メッセージへ整形する。
/// - covered 経路（work-channel 出力）のため redaction/masking は一切行わない。
///   command/args/result/stdout/stderr/rejection をそのまま載せる
///   （docs/design-webhook-output-lossless.md §2 P4）。
/// - Discord の 1 通上限に収まれば 1 通。超える場合のみ `part X/N` を付けて順序通りに
///   分割し、ロスレスに送る（head/tail/midpoint の切り捨てはしない）。順序は配送 worker
///   （単一 run = 単一 mpsc）が保証する。
pub fn build_tool_event_message(view: &ToolEventView) -> Vec<String> {
    let emoji = match view.status.as_str() {
        "started" => "▶️",
        "completed" => "✅",
        "failed" => "❌",
        "rejected" => "🚫",
        _ => "ℹ️",
    };
    let mut s = format!(
        "{emoji} **{}**\ntool: `{}`\ncallId: `{}`\ndepth: {}",
        view.event, view.tool_name, view.tool_call_id, view.depth
    );
    if let Some(d) = view.duration_ms {
        s.push_str(&format!("\nduration: {d}ms"));
    }
    if let Some(code) = view.exit_code {
        s.push_str(&format!("\nexit_code: `{code}`"));
    }
    if let Some(reason) = &view.rejection_reason {
        s.push_str(&format!("\nrejection: {reason}"));
    }
    if let Some(args) = &view.args_summary {
        s.push_str(&format!("\nargs: {args}"));
    }
    if let Some(res) = &view.result_summary {
        s.push_str(&format!("\nresult: {res}"));
    }
    if let Some(out) = &view.stdout_summary {
        s.push_str(&format!("\nstdout:\n{out}"));
    }
    if let Some(errout) = &view.stderr_summary {
        s.push_str(&format!("\nstderr:\n{errout}"));
    }
    if view.truncated {
        // 上流（execute_shell の max_output_bytes 等）が webhook 層より前に切り捨てた
        // 部分出力。完全だと偽らず、partial であることを明示する（P5/AC5）。
        s.push_str(
            "\n⚠️ partial output: the tool truncated this before the webhook layer saw the \
             full data (upstream source limit); the omitted bytes are not available here.",
        );
    }
    // ロスレス配送: 1 通に収まればそのまま、Discord 上限を超える場合のみ順序分割する。
    if s.chars().count() <= DISCORD_MESSAGE_LIMIT {
        return vec![s];
    }
    // chunk サイズは max_chars（プレビュー上限のヒント）を尊重しつつ Discord 安全長で頭打ち。
    // どちらでもロスは発生しない（分割するだけ）。
    let chunk_size = if view.max_chars == 0 {
        DISCORD_CHUNK_LIMIT
    } else {
        view.max_chars.min(DISCORD_CHUNK_LIMIT).max(1)
    };
    let parts = chunk_text(&s, chunk_size);
    let total = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, c)| format!("part {}/{}\n{}", i + 1, total, c))
        .collect()
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
    fn test_exit_reason_to_status_mapping() {
        assert_eq!(exit_reason_to_status("completed"), "completed");
        assert_eq!(exit_reason_to_status("stopped_by_limit"), "completed");
        assert_eq!(exit_reason_to_status("error"), "failed");
        assert_eq!(exit_reason_to_status("timeout"), "timed_out");
    }

    // ---- shell result summary ----

    #[test]
    fn test_summarize_shell_result_preserves_output_unredacted() {
        // covered 経路: stdout は redact せずバイト一致で保持する（新要件 §2 P4）。
        let data = json!({
            "exit_code": 0,
            "stdout": "ok API_KEY=supersecretvalue done",
            "stderr": "",
            "truncated": false,
        });
        let s = summarize_shell_result(&data);
        assert_eq!(s.exit_code, Some(0));
        assert!(!s.truncated);
        let out = s.stdout_summary.unwrap();
        assert_eq!(out, "ok API_KEY=supersecretvalue done");
        assert!(!out.contains("[REDACTED]"), "masking marker leaked: {out}");
        assert!(s.stderr_summary.is_none());
    }

    #[test]
    fn test_summarize_shell_result_does_not_clamp_long_output() {
        // 長い出力は head/tail クランプせず full を保持する（ロスレス）。
        let long = "x".repeat(5000);
        let data = json!({ "exit_code": 1, "stdout": long.clone(), "truncated": true });
        let s = summarize_shell_result(&data);
        assert!(s.truncated);
        let out = s.stdout_summary.unwrap();
        assert_eq!(out.chars().count(), 5000);
        assert_eq!(out, long);
        assert!(
            !out.contains("omitted"),
            "must not insert omission marker: {out}"
        );
    }

    // ---- tool event formatting ----

    #[test]
    fn test_build_tool_event_message_preserves_secrets_unredacted() {
        // covered 経路: args/stdout 中のシークレット・webhook URL はそのまま残す。
        let view = ToolEventView {
            event: "tool_call_completed".to_string(),
            tool_name: "execute_shell".to_string(),
            tool_call_id: "c1".to_string(),
            depth: 1,
            status: "completed".to_string(),
            duration_ms: Some(42),
            args_summary: Some("cmd: `echo API_KEY=supersecretvalue`".to_string()),
            result_summary: None,
            exit_code: Some(0),
            stdout_summary: Some(
                "token ghp_0123456789abcdefghij hook https://discord.com/api/webhooks/123/abcdefSECRETtoken"
                    .to_string(),
            ),
            stderr_summary: None,
            truncated: false,
            rejection_reason: None,
            max_chars: 1500,
        };
        let msgs = build_tool_event_message(&view);
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert!(m.contains("tool_call_completed"));
        assert!(m.contains("execute_shell"));
        assert!(m.contains("exit_code"));
        // unredacted: every secret-like string survives byte-for-byte.
        assert!(
            m.contains("API_KEY=supersecretvalue"),
            "kv secret stripped: {m}"
        );
        assert!(
            m.contains("ghp_0123456789abcdefghij"),
            "prefix secret stripped: {m}"
        );
        assert!(
            m.contains("https://discord.com/api/webhooks/123/abcdefSECRETtoken"),
            "webhook url stripped: {m}"
        );
        // no OpenCrab masking markers anywhere.
        assert!(!m.contains("[REDACTED]"), "REDACTED marker present: {m}");
        assert!(!m.contains("[redacted]"), "redacted marker present: {m}");
    }

    #[test]
    fn test_build_tool_event_message_chunks_long_output_losslessly() {
        // 長い出力は head/tail/midpoint で捨てず、part X/N で順序分割して全文を運ぶ。
        let stdout = "z".repeat(5000);
        let view = ToolEventView {
            event: "tool_call_completed".to_string(),
            tool_name: "execute_shell".to_string(),
            tool_call_id: "c".to_string(),
            depth: 1,
            status: "completed".to_string(),
            stdout_summary: Some(stdout.clone()),
            max_chars: 1500,
            ..Default::default()
        };
        let msgs = build_tool_event_message(&view);
        assert!(
            msgs.len() > 1,
            "long output must be split into multiple parts"
        );
        // every part is within Discord's hard limit and labelled in order.
        for (i, m) in msgs.iter().enumerate() {
            assert!(
                m.chars().count() <= DISCORD_MESSAGE_LIMIT,
                "part too long: {}",
                m.len()
            );
            assert!(
                m.starts_with(&format!("part {}/{}\n", i + 1, msgs.len())),
                "part marker/order wrong: {m}"
            );
        }
        // reconstruct: strip the one-line part marker from each, concat -> full message.
        let reconstructed: String = msgs
            .iter()
            .map(|m| m.splitn(2, '\n').nth(1).unwrap_or("").to_string())
            .collect();
        assert!(
            reconstructed.contains(&stdout),
            "reconstruction lost stdout"
        );
        // no ellipsis/omission/masking introduced.
        assert!(!reconstructed.contains('…'), "ellipsis introduced");
        assert!(!reconstructed.contains("[REDACTED]"));
        // the full 5000 chars are present.
        assert_eq!(reconstructed.matches('z').count(), 5000);
    }

    #[test]
    fn test_build_tool_event_message_surfaces_upstream_truncation() {
        // 上流由来の切り捨ては「完全」と偽らず partial と明示する（AC5）。
        let view = ToolEventView {
            event: "tool_call_completed".to_string(),
            tool_name: "execute_shell".to_string(),
            tool_call_id: "c".to_string(),
            depth: 1,
            status: "completed".to_string(),
            stdout_summary: Some("head".to_string()),
            truncated: true,
            max_chars: 1500,
            ..Default::default()
        };
        let msgs = build_tool_event_message(&view);
        let joined = msgs.join("");
        assert!(
            joined.contains("partial output"),
            "must mark partial: {joined}"
        );
    }

    // ---- live E2E (env-gated, #[ignore] by default) ----
    //
    // 実 DB の agent 既定 webhook を使って、空 explicit url がフォールバックして実際の
    // Discord webhook へ配送されることを確認する。raw url はコードに置かず、実行時に DB
    // から解決して使う（secret はログにも出さない）。
    //
    // 実行例:
    //   OPENCRAB_E2E_DB=data/opencrab.db \
    //   OPENCRAB_E2E_AGENT_ID=c56f19e0-... \
    //   cargo test -p opencrab-discord -- --ignored --nocapture e2e_empty_url_fallback_delivers
    #[tokio::test]
    #[ignore = "live: posts to a real Discord webhook; env-gated"]
    async fn e2e_empty_url_fallback_delivers() {
        let db_path = std::env::var("OPENCRAB_E2E_DB")
            .expect("set OPENCRAB_E2E_DB to an absolute path to the live opencrab.db");
        let agent_id = std::env::var("OPENCRAB_E2E_AGENT_ID")
            .expect("set OPENCRAB_E2E_AGENT_ID to the agent with a configured default webhook");

        // resolve_subtask_webhook は SELECT のみ（書き込まない）。WAL の live DB に対して
        // read-only open は CannotOpen になり得るため通常 open する。
        let conn = rusqlite::Connection::open(&db_path).expect("open real DB");

        // (1) 空 explicit url → デフォルトへフォールバックして Use になる。
        let empty_args = json!({ "webhook": { "url": "" } });
        let resolved =
            resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &empty_args, None);
        let (url, source) = match resolved {
            WebhookResolution::Use { config, source } => (config.url, source),
            other => panic!(
                "empty url must fall back to a default Use; got {:?}",
                match other {
                    WebhookResolution::None => "None",
                    WebhookResolution::Disabled { .. } => "Disabled",
                    WebhookResolution::Error { .. } => "Error",
                    WebhookResolution::Use { .. } => unreachable!(),
                }
            ),
        };
        assert!(
            matches!(
                source,
                WebhookSource::AgentDefault
                    | WebhookSource::ToolDefault
                    | WebhookSource::GlobalDefault
                    | WebhookSource::EnvConfig
            ),
            "fallback source should be a default, got {}",
            source.as_str()
        );
        // raw url は出さない。redacted のみ。
        eprintln!(
            "[e2e] empty url -> fallback source={} url={}",
            source.as_str(),
            redact_webhook_url(&url)
        );

        // 実 Discord webhook へ started lifecycle を 1 通配送する（実 HTTP）。
        let meta = LifecycleMeta {
            label: "E2E empty-url fallback".to_string(),
            run_id: "e2e-empty-url".to_string(),
            session_key: "e2e".to_string(),
        };
        let messages = build_started_messages(
            &meta,
            "E2E: empty explicit webhook url fell back to default (this is a test message).",
            DISCORD_CHUNK_LIMIT,
        );
        let client = reqwest::Client::new();
        for msg in &messages {
            let resp = client
                .post(&url)
                .json(&json!({ "content": msg }))
                .send()
                .await
                .expect("discord webhook POST should complete");
            assert!(
                resp.status().is_success(),
                "discord webhook should accept message: http {}",
                resp.status().as_u16()
            );
        }
        eprintln!(
            "[e2e] delivered {} message(s) to the default webhook",
            messages.len()
        );

        // (2) 非空の不正 url はフォールバックせず Error（strict 維持）。
        let bad_args =
            json!({ "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" } });
        let bad = resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &bad_args, None);
        match bad {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
                assert!(
                    !message.contains("evil.example.com"),
                    "raw url leaked: {message}"
                );
                eprintln!("[e2e] invalid explicit url -> Error (no fallback): {code}: {message}");
            }
            _ => panic!("non-empty invalid explicit url must Error, not fall back"),
        }
    }
}
