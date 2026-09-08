//! Subtask lifecycle webhook delivery.
//!
//! opencrab 本体の subtask lifecycle を authoritative な source of truth として、
//! started / completed / failed / timed_out / aborted を Discord webhook へ配送する。
//!
//! 設計: docs/subtask-webhook-tracking-design.md (Phase 1)
//!
//! - raw task text はそのまま送る（要約も redact もしない）
//! - 長文は **分割連投しない**。出だしのプレビューを本文に載せ、全文は
//!   multipart/form-data の添付ファイルとして **1 通**で送る（#293。閾値・プレビュー長・
//!   ファイル名・サイズ上限のポリシーは `opencrab_actions::webhook_target` が持つ）
//! - 同一 run の配送は 1 本の mpsc チャネル + 1 worker で直列化し、ordering を保証する
//!   （別 run の worker とは並行に動くため interleave しうるが、同一 run 内は順序維持）
//! - 429 は Retry-After を尊重し、その他失敗は best-effort backoff retry する

use std::time::Duration;

use tokio::sync::mpsc;

// 通知先（webhook）の設定型・解決・URL 検証・秘匿処理・テキスト分割は gateway 非依存層
// （`opencrab_actions::webhook_target`）へ移設済み（#157 S4）。この module に残るのは
// **実際の HTTP 配送（transport）と Discord 固有の整形**だけ。既存の呼び出し元が
// `webhook::...` のまま参照できるよう、Discord 側で使う項目はここで再エクスポートする
// （crate 内部向け: `mod webhook` は private なので Discord crate の公開 API には出ない）。
// 汎用の秘匿ユーティリティ `redact_secrets` は Discord 側に利用者が居ないため re-export せず、
// 必要なら `opencrab_actions::webhook_target::redact_secrets` を直接参照する。
pub use opencrab_actions::webhook_target::{
    build_message_with_attachment_preview, build_message_with_optional_attachment,
    build_webhook_body, has_activity_default, record_webhook_delivery_failure, redact_webhook_url,
    resolve_activity_webhook, resolve_subtask_webhook, WebhookConfig, WebhookMessage,
    WebhookResolution, WebhookSource,
};

/// 送信を最終的にあきらめたとき、短いエラー説明文字列で呼ばれる give-up sink。
type GiveupSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// 1 回の webhook POST に許すハングの上限。
///
/// Discord webhook の応答は通常 1 秒未満。添付付き（multipart）は本文全体をボディに載せる
/// ぶん明確に重いので、遅い回線でも 8 MiB を送り切れる余裕として 60 秒を取る。ここで必ず
/// 打ち切ることで、接続が黙って死んだときに配送 worker が永久待ちに入るのを防ぐ
/// （worker が止まるとその run の後続イベントが全部止まる）。JSON のみの送信は軽いので
/// 短く倒す。
const SEND_TIMEOUT_JSON: Duration = Duration::from_secs(30);
const SEND_TIMEOUT_MULTIPART: Duration = Duration::from_secs(60);

/// lifecycle イベントの共通メタ情報（payload 整形用）。
#[derive(Clone, Debug)]
pub struct LifecycleMeta {
    pub label: String,
    pub run_id: String,
    pub session_key: String,
}

/// 配送 worker に渡す 1 バッチ。messages は同一 run 内で順序通りに送る。
///
/// 1 要素 = 1 POST。長文はもう分割されないので、要素数は「メタ情報 + 本体」程度に収まる。
#[derive(Clone, Debug)]
pub struct DeliveryBatch {
    pub url: String,
    pub messages: Vec<WebhookMessage>,
}

/// started 用のメッセージ列を組み立てる。
///
/// 開始通知を **1 通・ヘッダ付き**で組む（`🟢 **subtask started**` + メタ + `task:` 本体）。
///
/// task 本体はインラインで畳み、長ければ**出だしのプレビュー + 全文添付**の 1 通にする
/// （#293。従来の `part X/N` 連投はやめた・全文は添付にロスなく入る）。task が空なら
/// `task:` 行は付かない。返り値は互換のため `Vec` だが常に 1 要素。
///
/// 以前は「メタ 1 通 + raw task text 本体 1 通」の 2 通で、本体はヘッダ無しだったため、
/// 自己受信 drop（lifecycle payload 判定 = 本文 1 行目が `... **subtask <status>**` ヘッダ）を
/// **すり抜けて幽霊ターンを 1 発起こし得た**。terminal / progress と同じ「1 通・ヘッダ付き」に
/// 揃えることで headerless な lifecycle メッセージを構造的に無くす。プレビューは常にヘッダを
/// 含む（ヘッダは短く、preview_chars 内に必ず収まる）ので、長文添付でも 1 行目はヘッダのまま。
pub fn build_started_messages(meta: &LifecycleMeta, raw_task_text: &str) -> Vec<WebhookMessage> {
    let mut s = format!(
        "🟢 **subtask started**\nlabel: `{}`\nrunId: `{}`\nsessionKey: `{}`\nstatus: `started`",
        meta.label, meta.run_id, meta.session_key
    );
    if !raw_task_text.is_empty() {
        s.push_str("\ntask: ");
        s.push_str(raw_task_text);
    }
    vec![build_message_with_optional_attachment(
        &s,
        "subtask-started",
    )]
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
pub fn spawn_run_worker_with_sink(
    client: reqwest::Client,
    on_giveup: Option<GiveupSink>,
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

/// 1 メッセージ（本文 + 任意の添付）を Discord webhook へ **1 POST** で送る。
/// 429 は Retry-After を尊重し、その他失敗は best-effort backoff retry する。
///
/// 添付があるときは multipart/form-data（`payload_json` + `files[0]`）で送る。添付が
/// 4xx（413 Payload Too Large / 400 等、429 を除く）で弾かれた場合だけは、同じ body を
/// 何度投げても通らないので **添付を落として本文（プレビュー）だけを JSON で送り直す**
/// ところまで劣化させる。要旨が Discord に残るほうが「全部消える」より良いため。
/// 5xx / ネットワークエラーは従来どおりそのまま backoff retry する。
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    message: &WebhookMessage,
    on_giveup: Option<&GiveupSink>,
) {
    // 即時 / 2s / 10s / 30s / 120s
    const BACKOFFS: [u64; 5] = [0, 2, 10, 30, 120];
    let mut attempt = 0usize;
    // covered 経路（配送 debug/log）では webhook URL をマスクしない。URL がマスクされると
    // 配送先の特定・障害切り分けが困難になりデバッグ性を損なうため、生 URL をそのまま記録する
    // （docs/design-webhook-output-lossless.md §2 P4: 漏洩時は webhook を無効化して回復する）。
    let mut last_error;
    let mut attachment = message.attachment.as_ref();
    loop {
        let body = build_webhook_body(&message.content, false);
        let req = match attachment {
            Some(att) => {
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.filename.clone())
                    .mime_str(&att.content_type)
                    .unwrap_or_else(|_| {
                        reqwest::multipart::Part::bytes(att.data.clone())
                            .file_name(att.filename.clone())
                    });
                // Discord webhook の multipart 仕様: メッセージ本体は `payload_json`、
                // 添付は `files[0]`（複数なら files[1]...）。
                let form = reqwest::multipart::Form::new()
                    .text("payload_json", body.to_string())
                    .part("files[0]", part);
                client
                    .post(url)
                    .timeout(SEND_TIMEOUT_MULTIPART)
                    .multipart(form)
            }
            None => client.post(url).timeout(SEND_TIMEOUT_JSON).json(&body),
        };
        match req.send().await {
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
                if attachment.is_some() && status.is_client_error() {
                    // 添付が拒否された。再送しても同じなので添付を捨て、本文だけで続行する。
                    tracing::warn!(
                        url = %url,
                        status = status.as_u16(),
                        "discord webhook rejected the attachment; retrying without it (preview only)"
                    );
                    attachment = None;
                    continue;
                }
            }
            Err(e) => {
                last_error = if e.is_timeout() {
                    "timeout".to_string()
                } else {
                    "request error".to_string()
                };
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
    /// 本文プレビューの上限ヒント（0 なら既定）。#293 以降、長文は分割ではなく添付に
    /// 畳まれるため、この値は**プレビューを既定より更に短くしたいとき**にだけ効く
    /// （既定 `ATTACHMENT_PREVIEW_CHARS` との小さいほうを採る）。
    pub max_chars: usize,
}

/// ツールイベントを Discord 用メッセージへ整形する。
/// - covered 経路（work-channel 出力）のため redaction/masking は一切行わない。
///   command/args/result/stdout/stderr/rejection をそのまま載せる
///   （docs/design-webhook-output-lossless.md §2 P4）。
/// - Discord の 1 通上限に収まれば従来どおり JSON 1 通。超える場合は **分割連投せず**、
///   出だしのプレビューを本文に載せて**全文を添付ファイルにした 1 通**を返す（#293）。
///   ロスレス性は維持される（全文は添付に入る。上限超過分の扱いは
///   `ATTACHMENT_MAX_BYTES` の doc 参照）。
/// - 添付本体は本文と**同じ文字列**から作る。したがって stdout/stderr に上流で掛かって
///   いるマスク（nostr の `nsec` マスク等）は添付側にもそのまま効く。添付だけが別経路で
///   生データを拾うことは構造的に起きない。
pub fn build_tool_event_message(view: &ToolEventView) -> Vec<WebhookMessage> {
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
    // ロスレス配送: 1 通に収まればそのまま JSON で。超える場合は
    // 「出だしのプレビュー + 全文添付」の 1 通に畳む（分割連投しない / #293）。
    // ファイル名には静的な語彙（イベント名 + ツール名）だけを載せる。callId や引数は
    // 秘密・個人情報を含みうるので名前には入れない（中身は添付本体に入る）。
    // max_chars（webhook 設定の出力上限）はプレビューを既定より短くする方向にだけ効く。
    vec![build_message_with_attachment_preview(
        &s,
        &format!("{}-{}", view.event, view.tool_name),
        view.max_chars,
    )]
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
#[path = "webhook/tests.rs"]
mod tests;
