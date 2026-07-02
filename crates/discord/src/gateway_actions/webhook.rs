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
    /// `events` は省略可能。`url` が無い / 空 / 空白のみなら「明示指定なし」として
    /// None を返す（呼び出し側はデフォルトへフォールバックできる）。
    pub fn from_args(args: &serde_json::Value) -> Option<WebhookConfig> {
        let wh = args.get("webhook")?;
        let url = wh.get("url").and_then(|v| v.as_str())?.to_string();
        if url.trim().is_empty() {
            return None;
        }
        let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        Some(WebhookConfig { url, events })
    }

    pub fn from_parts(url: String, events: Option<Vec<String>>) -> Option<WebhookConfig> {
        if url.trim().is_empty() {
            return None;
        }
        Some(WebhookConfig { url, events })
    }

    /// 指定イベントを送るべきか。events 未指定なら常に true。
    pub fn wants(&self, event: &str) -> bool {
        match &self.events {
            Some(list) => {
                // 比較は canonical な status 名で行う。depth0 sink は
                // `tool_call_started`/`tool_call_completed`/... を、subtask path は
                // `subtask.started`/`started`/... を渡してくるため、両辺を正規化して
                // 同じ語彙（started/completed/failed/rejected/...）で突き合わせる。
                let want = normalize_event_name(event);
                if list.iter().any(|e| normalize_event_name(e) == want) {
                    return true;
                }
                // Backward compatibility for callers that created lifecycle streams before
                // progress existed: started/completed streams should include tool progress too.
                want == "progress" && list.iter().any(|e| normalize_event_name(e) == "started")
            }
            None => true,
        }
    }
}

/// イベント名を canonical な status 名へ正規化する。
/// `subtask.` 接頭辞（subtask lifecycle）と `tool_call_` 接頭辞（depth0 tool sink）を剥がし、
/// `started`/`completed`/`failed`/`rejected`/`timed_out`/`progress` 等の素の status に揃える。
fn normalize_event_name(event: &str) -> &str {
    event
        .strip_prefix("subtask.")
        .or_else(|| event.strip_prefix("tool_call_"))
        .unwrap_or(event)
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

/// raw text を `part X/N` 付きメッセージ列に整形する。
///
/// この framing はピアレビュー等で「part X/N の生データを読め」という
/// プロンプト規約とセットのプロトコルなので、変更時は全利用箇所と
/// system prompt（server/process.rs）を同時に更新すること。
pub fn build_part_messages(content: &str, limit: usize) -> Vec<String> {
    let chunks = chunk_text(content, limit);
    let part_count = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("part {}/{}\n{}", i + 1, part_count, c))
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

/// webhook URL のトークン（末尾セグメント）をマスクして返す。ログ・応答用。
pub fn redact_webhook_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/[redacted]"),
        None => "[redacted]".to_string(),
    }
}

// ---- Secret redaction (retained utility) ----
//
// 本設計（docs/design-webhook-output-lossless.md §2 P4）により、covered 経路
// （work-channel 出力: command/stdout/stderr/args/result）からは redaction を完全に外した。
// 以下の関数群はもはや配送経路では呼ばれないが、covered 経路外（別タスク・§8）で再利用しうる
// 汎用ユーティリティとして残す。未使用でも警告を出さないため allow(dead_code) を付ける。

#[allow(dead_code)]
const REDACTED: &str = "[REDACTED]";
#[allow(dead_code)]
const SECRET_PREFIXES: [&str; 4] = ["sk-", "ghp_", "xoxb-", "AKIA"];
#[allow(dead_code)]
const KV_MARKERS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "API"];

/// 既知のシークレットパターンを [REDACTED] に置換する汎用ユーティリティ。
/// 取りこぼし対策として保守的に倒す（長い base64/hex 連や Bearer トークンも redact）。
/// 冪等: 既に redact 済みの文字列を再度通しても安全。
/// 注: covered 経路（webhook 出力）では **呼ばない**（§2 P4）。
#[allow(dead_code)]
pub fn redact_secrets(input: &str) -> String {
    input
        .split('\n')
        .map(redact_secrets_line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(dead_code)]
fn redact_secrets_line(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for tok in line.split_whitespace() {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if tok.eq_ignore_ascii_case("bearer") {
            out.push(tok.to_string());
            redact_next = true;
            continue;
        }
        let (rendered, want_next) = redact_secret_token(tok);
        out.push(rendered);
        redact_next = want_next;
    }
    out.join(" ")
}

/// 1 トークンを検査し、(置換後文字列, 次トークンも redact すべきか) を返す。
#[allow(dead_code)]
fn redact_secret_token(tok: &str) -> (String, bool) {
    let core = tok.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | ';' | '(' | ')' | '`' | '[' | ']' | '{' | '}'
        )
    });
    if core.is_empty() {
        return (tok.to_string(), false);
    }
    // Discord webhook URL（ホスト不問）
    if core.contains("/api/webhooks/") {
        return (REDACTED.to_string(), false);
    }
    // KEY=VALUE / KEY:VALUE （キーに TOKEN/SECRET/PASSWORD/KEY/API を含む）
    if let Some(idx) = core.find(|c: char| c == '=' || c == ':') {
        let (k, rest) = core.split_at(idx);
        let delim = &core[idx..idx + 1];
        let value = &rest[1..];
        let key_up = k.trim_matches('"').to_ascii_uppercase();
        if KV_MARKERS.iter().any(|m| key_up.contains(m)) {
            if value.trim().is_empty() {
                // 値は次トークン側にある（例: `"token": "abc"`）
                return (tok.to_string(), true);
            }
            return (format!("{k}{delim}{REDACTED}"), false);
        }
    }
    // 既知プレフィックス
    for p in SECRET_PREFIXES {
        if core.starts_with(p) && core.len() > p.len() + 3 {
            return (REDACTED.to_string(), false);
        }
    }
    // 長い base64 / hex 連
    if core.len() >= 32
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
    {
        return (REDACTED.to_string(), false);
    }
    (tok.to_string(), false)
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

/// Discord webhook URL を検証する。空・パース不可・Discord webhook でない場合は Err(理由)。
///
/// 理由文字列に raw URL は含めない。
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url is empty".to_string());
    }
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return Err("url must start with https://".to_string()),
    };
    // host = "https://" と最初の '/' の間の部分。
    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => return Err("url has no path".to_string()),
    };
    const ALLOWED_HOSTS: [&str; 4] = [
        "discord.com",
        "discordapp.com",
        "ptb.discord.com",
        "canary.discord.com",
    ];
    if !ALLOWED_HOSTS.contains(&host) {
        return Err("host is not a Discord webhook host".to_string());
    }
    let webhook_path = match path.strip_prefix("/api/webhooks/") {
        Some(p) => p,
        None => return Err("path must start with /api/webhooks/".to_string()),
    };
    // id / token の 2 つ以上の非空セグメントが必要。
    let segments: Vec<&str> = webhook_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err("path is missing webhook id or token".to_string());
    }
    Ok(())
}

/// subtask webhook の解決元（優先順位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookSource {
    Explicit,
    ToolDefault,
    AgentDefault,
    GlobalDefault,
    EnvConfig,
}

impl WebhookSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookSource::Explicit => "explicit",
            WebhookSource::ToolDefault => "tool_default",
            WebhookSource::AgentDefault => "agent_default",
            WebhookSource::GlobalDefault => "global_default",
            WebhookSource::EnvConfig => "env_config",
        }
    }
}

/// subtask webhook の解決結果。
pub enum WebhookResolution {
    /// 検証済みの webhook。ここへ配送する。
    Use {
        config: WebhookConfig,
        source: WebhookSource,
    },
    /// 当選した scope で enabled=false。webhook 無効・fallthrough しない。
    Disabled { source: WebhookSource },
    /// どこにも設定が無い。
    None,
    /// 検証失敗 → spawn_subtask を失敗させる。
    Error {
        code: String,
        message: String,
        source: WebhookSource,
    },
}

/// events_json (Option<String>) から events を解析する。
fn parse_events_json(events_json: &Option<String>) -> Option<Vec<String>> {
    let raw = events_json.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let arr = value.as_array()?;
    Some(
        arr.iter()
            .filter_map(|e| e.as_str().map(|s| s.to_string()))
            .collect(),
    )
}

/// DB 行を WebhookResolution へ変換する（enabled/url 検証含む）。
fn resolve_db_row(row: AgentWebhookConfigRowLite, source: WebhookSource) -> WebhookResolution {
    if !row.enabled {
        return WebhookResolution::Disabled { source };
    }
    if let Err(reason) = validate_webhook_url(&row.url) {
        return WebhookResolution::Error {
            code: "invalid_default_webhook".to_string(),
            message: reason,
            source,
        };
    }
    let events = parse_events_json(&row.events_json);
    WebhookResolution::Use {
        config: WebhookConfig {
            url: row.url,
            events,
        },
        source,
    }
}

/// resolve で必要な DB 行の最小フィールド。
struct AgentWebhookConfigRowLite {
    url: String,
    events_json: Option<String>,
    enabled: bool,
}

/// 1 つの scope について指定 kind 群を順に試し、最初に見つかった行を返す。
fn fetch_scope_row_kinds(
    conn: &rusqlite::Connection,
    scope: &str,
    agent_id: &str,
    tool_name: &str,
    kinds: &[&str],
) -> Option<AgentWebhookConfigRowLite> {
    for kind in kinds {
        if let Ok(Some(r)) =
            opencrab_db::queries::get_agent_webhook_config(conn, scope, agent_id, tool_name, kind)
        {
            return Some(AgentWebhookConfigRowLite {
                url: r.url,
                events_json: r.events_json,
                enabled: r.enabled,
            });
        }
    }
    None
}

/// 1 つの scope について subtask lifecycle の宛先行を取得する。
///
/// 優先順位は `subtask > lifecycle > activity`。subtask 専用に設定された明示的な
/// デフォルト（subtask/lifecycle kind）を、汎用 activity デフォルトより優先する。
/// activity family は subtask ライフサイクルも包含するため、subtask 専用行が無い
/// ときのフォールバックとして最後に見る。
fn fetch_scope_row(
    conn: &rusqlite::Connection,
    scope: &str,
    agent_id: &str,
    tool_name: &str,
) -> Option<AgentWebhookConfigRowLite> {
    fetch_scope_row_kinds(conn, scope, agent_id, tool_name, &["subtask", "lifecycle", "activity"])
}

/// subtask webhook を固定順序で解決する。
///
/// 優先順位: explicit > tool default > agent default > global default > env config。
/// あるレベルで設定が見つかったら、それより下へは fall through しない
/// （error/disabled も同様に止まる）。
pub fn resolve_subtask_webhook(
    conn: &rusqlite::Connection,
    agent_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    env_config_default: Option<&WebhookConfig>,
) -> WebhookResolution {
    // 1. EXPLICIT
    // webhook キーがあり、url が非空（trim 後）のときだけ明示指定として扱う。
    // url が空文字 / 空白のみのときは「明示指定なし」とみなし、下位のデフォルト解決へ
    // フォールバックさせる（明示的に空 url を渡しても通知が無効化されない）。これは DB の
    // enabled=false による明示無効化（auditable disable）とは別物で、後者はその scope で
    // 配送を止め fall through しない。
    if let Some(wh) = args.get("webhook") {
        if !wh.is_null() {
            let url = wh
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !url.trim().is_empty() {
                if let Err(reason) = validate_webhook_url(&url) {
                    return WebhookResolution::Error {
                        code: "invalid_webhook_url".to_string(),
                        message: reason,
                        source: WebhookSource::Explicit,
                    };
                }
                let events = wh.get("events").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|e| e.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });
                return WebhookResolution::Use {
                    config: WebhookConfig {
                        url: url.trim().to_string(),
                        events,
                    },
                    source: WebhookSource::Explicit,
                };
            }
            // url 空 / 空白のみ → 明示指定なし扱い。下の DB / env デフォルトへ続行する。
        }
    }

    // 2. DB defaults: tool > agent > global。最初に見つかった行で確定。
    if let Some(row) = fetch_scope_row(conn, "tool", agent_id, "spawn_subtask") {
        return resolve_db_row(row, WebhookSource::ToolDefault);
    }
    if let Some(row) = fetch_scope_row(conn, "agent", agent_id, "") {
        return resolve_db_row(row, WebhookSource::AgentDefault);
    }
    if let Some(row) = fetch_scope_row(conn, "global", "*", "") {
        return resolve_db_row(row, WebhookSource::GlobalDefault);
    }

    // 3. env/config 互換フォールバック。DB 行が皆無のときのみ。
    let _ = tool_name;
    match env_config_default {
        Some(cfg) => WebhookResolution::Use {
            config: cfg.clone(),
            source: WebhookSource::EnvConfig,
        },
        None => WebhookResolution::None,
    }
}

/// 一般ツール/コマンド活動（activity family）の宛先を固定順序で解決する。
///
/// 優先順位: tool-specific(activity) > agent(activity) > global(activity)。
/// 明示 per-call webhook も env/config fallback も用いない（design 2.2: env/config は
/// subtask ファミリ限定）。activity kind の DB 行のみを見る。
/// disabled / 不正 URL は下位へ fall through しない（no-silent-fallback）。
pub fn resolve_activity_webhook(
    conn: &rusqlite::Connection,
    agent_id: &str,
    tool_name: &str,
) -> WebhookResolution {
    if !tool_name.is_empty() {
        if let Some(row) =
            fetch_scope_row_kinds(conn, "tool", agent_id, tool_name, &["activity"])
        {
            return resolve_db_row(row, WebhookSource::ToolDefault);
        }
    }
    if let Some(row) = fetch_scope_row_kinds(conn, "agent", agent_id, "", &["activity"]) {
        return resolve_db_row(row, WebhookSource::AgentDefault);
    }
    if let Some(row) = fetch_scope_row_kinds(conn, "global", "*", "", &["activity"]) {
        return resolve_db_row(row, WebhookSource::GlobalDefault);
    }
    WebhookResolution::None
}

/// agent に適用され得る有効な activity デフォルトが 1 つでも存在するか。
///
/// `resolve_activity_webhook` と同じ scope 集合（tool / agent / global の activity 行）を
/// 見る。`list_agent_webhook_config` は `(agent_id = ? OR agent_id = '*') AND enabled = 1`
/// で引くため、agent 自身の tool/agent scope 行と global(`*`) 行を enabled のみ含む。
/// env/config fallback は使わない（activity kind の DB 行のみ）。
/// 配送 sink を立てる価値があるか（best-effort）の単一判定点。
pub fn has_activity_default(conn: &rusqlite::Connection, agent_id: &str) -> bool {
    opencrab_db::queries::list_agent_webhook_config(conn, Some(agent_id), false)
        .map(|rows| rows.iter().any(|r| r.kind == "activity"))
        .unwrap_or(false)
}

/// webhook 配送が最終的に失敗したとき、親セッションログに 1 件記録する。
///
/// raw url は決して渡さない（redacted_url のみ）。parent_session_id が空なら何もしない。
pub fn record_webhook_delivery_failure(
    conn: &rusqlite::Connection,
    agent_id: &str,
    parent_session_id: &str,
    subtask_id: &str,
    sub_session_id: &str,
    redacted_url: &str,
    error: &str,
) {
    if parent_session_id.is_empty() {
        return;
    }
    let content = json!({
        "type": "subtask_progress",
        "subtask_id": subtask_id,
        "session_id": sub_session_id,
        "webhook_status": "delivery_failed",
        "webhook_redacted_url": redacted_url,
        "webhook_error": error,
    })
    .to_string();
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: parent_session_id.to_string(),
        log_type: "system".to_string(),
        content,
        speaker_id: None,
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(conn, &log).ok();
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
        // 空白のみの url も「指定なし」として None（フォールバック可能）。
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "   " } })).is_none());
    }

    #[test]
    fn test_webhook_config_from_parts_missing_or_empty() {
        assert!(WebhookConfig::from_parts("".to_string(), None).is_none());
        assert!(WebhookConfig::from_parts("   ".to_string(), None).is_none());

        let cfg = WebhookConfig::from_parts(
            "https://discord.com/api/webhooks/x".to_string(),
            Some(vec!["started".to_string()]),
        )
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(cfg.events, Some(vec!["started".to_string()]));
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

        // Regression: depth0 sink emits `tool_call_*`; the stored allow-list uses the
        // canonical status vocabulary. Both sides must normalize to the same token so
        // activity events are not silently dropped before HTTP delivery.
        let activity_legacy = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec![
                "started".to_string(),
                "progress".to_string(),
                "completed".to_string(),
                "failed".to_string(),
                "timed_out".to_string(),
            ]),
        };
        assert!(activity_legacy.wants("tool_call_started"));
        assert!(activity_legacy.wants("tool_call_completed"));
        assert!(activity_legacy.wants("tool_call_failed"));
        // `rejected` is a tool-only status absent from this legacy list, so it stays
        // filtered here; an all-events (None) config delivers it.
        assert!(!activity_legacy.wants("tool_call_rejected"));

        let activity_explicit = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["rejected".to_string(), "tool_call_failed".to_string()]),
        };
        assert!(activity_explicit.wants("tool_call_rejected"));
        assert!(activity_explicit.wants("tool_call_failed"));
        assert!(!activity_explicit.wants("tool_call_started"));
    }

    #[test]
    fn test_exit_reason_to_status_mapping() {
        assert_eq!(exit_reason_to_status("completed"), "completed");
        assert_eq!(exit_reason_to_status("stopped_by_limit"), "completed");
        assert_eq!(exit_reason_to_status("error"), "failed");
        assert_eq!(exit_reason_to_status("timeout"), "timed_out");
    }

    // ---- webhook URL validation ----

    const VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcdefSECRETtoken";
    const SECRET_TOKEN: &str = "abcdefSECRETtoken";

    #[test]
    fn test_validate_webhook_url_valid() {
        assert!(validate_webhook_url(VALID_URL).is_ok());
        assert!(
            validate_webhook_url("https://canary.discord.com/api/webhooks/1/tok").is_ok()
        );
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/1/tok").is_ok());
        assert!(validate_webhook_url("https://ptb.discord.com/api/webhooks/1/tok").is_ok());
    }

    #[test]
    fn test_validate_webhook_url_invalid() {
        assert!(validate_webhook_url("").is_err());
        assert!(validate_webhook_url("   ").is_err());
        assert!(validate_webhook_url("http://discord.com/api/webhooks/1/tok").is_err());
        assert!(validate_webhook_url("https://evil.com/api/webhooks/1/tok").is_err());
        // missing token segment
        assert!(validate_webhook_url("https://discord.com/api/webhooks/123").is_err());
        // wrong path
        assert!(validate_webhook_url("https://discord.com/channels/1/2").is_err());
        // no path
        assert!(validate_webhook_url("https://discord.com").is_err());
        // reason must not leak the raw url
        let reason = validate_webhook_url("https://evil.com/api/webhooks/1/secrettok").unwrap_err();
        assert!(!reason.contains("secrettok"));
    }

    // ---- redaction ----

    #[test]
    fn test_redact_webhook_url_hides_token() {
        let redacted = redact_webhook_url(VALID_URL);
        assert!(!redacted.contains(SECRET_TOKEN), "token leaked: {redacted}");
        assert!(redacted.contains("[redacted]"));
        assert!(redacted.contains("123456789"));
    }

    // ---- resolution ----

    fn insert_row(
        conn: &rusqlite::Connection,
        scope: &str,
        agent_id: &str,
        tool_name: &str,
        kind: &str,
        url: &str,
        enabled: bool,
    ) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: scope.to_string(),
            agent_id: agent_id.to_string(),
            tool_name: tool_name.to_string(),
            kind: kind.to_string(),
            url: url.to_string(),
            events_json: None,
            enabled,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    fn use_source(r: &WebhookResolution) -> WebhookSource {
        match r {
            WebhookResolution::Use { source, .. } => *source,
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_webhook_resolution_explicit_beats_db() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": VALID_URL } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::Explicit);
    }

    #[test]
    fn test_webhook_resolution_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(&conn, "tool", "a1", "spawn_subtask", "subtask", VALID_URL, true);
        let args = json!({});
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        // remove tool -> agent wins
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        // only global
        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "subtask", VALID_URL, true);
        let r3 = resolve_subtask_webhook(&conn3, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_webhook_resolution_db_beats_env_config() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: "https://discord.com/api/webhooks/9/envtok".to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_env_only_when_no_db_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_invalid_explicit() {
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "http://evil.com/x" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
            }
            _ => panic!("expected Error"),
        }
    }

    // ---- empty / whitespace explicit url falls back to default (not an error) ----

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_db_default() {
        // 明示 webhook の url が空文字なら「指定なし」扱いとし、DB の agent デフォルトへ
        // フォールバックする（Error にして配送をブロックしない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_whitespace_explicit_url_falls_back_to_db_default() {
        // 空白のみの url も「指定なし」扱い。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_env_config() {
        // DB 行が無くても、空 url は env/config デフォルトへフォールバックする。
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_with_no_default_is_none() {
        // 空 url + デフォルト無し → None（Error ではない）。
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_keeps_events_ignored_on_fallback() {
        // 空 url のとき explicit events は使われず、フォールバック先（DB）の設定が勝つ。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "", "events": ["completed"] } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(config.url, VALID_URL);
                // DB 行は events_json=None なので全イベント送信。
                assert_eq!(config.events, None);
            }
            _ => panic!("expected Use from DB default"),
        }
    }

    #[test]
    fn test_webhook_resolution_nonempty_invalid_explicit_still_errors_over_default() {
        // 非空の不正 url はデフォルトがあっても fall through せず Error（strict 維持）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "http://evil.com/x/secrettok" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
                // 診断メッセージに raw url/token は漏れない。
                assert!(!message.contains("secrettok"), "token leaked: {message}");
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_invalid_db_default_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool default invalid, agent default valid -> must NOT fall through.
        insert_row(&conn, "tool", "a1", "spawn_subtask", "subtask", "http://bad", true);
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool disabled, agent valid -> Disabled, no fallthrough.
        insert_row(&conn, "tool", "a1", "spawn_subtask", "subtask", VALID_URL, false);
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Disabled { source } => {
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Disabled, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_lifecycle_alias() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a 'lifecycle' row at agent scope -> resolves like subtask.
        insert_row(&conn, "agent", "a1", "", "lifecycle", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);

        // lifecycle at tool scope still beats agent-scope subtask row.
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(&conn2, "tool", "a1", "spawn_subtask", "lifecycle", VALID_URL, true);
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r2), WebhookSource::ToolDefault);
    }

    // ---- secret redaction ----

    #[test]
    fn test_redact_secrets_scrubs_known_patterns() {
        let input = "key sk-ABCDEFGHIJKLMNOP and ghp_0123456789abcdefghij and AKIAABCDEFGHIJKLMNOP \
                     Authorization: Bearer myreallylongtoken123456 \
                     API_KEY=supersecretvalue \
                     hook https://discord.com/api/webhooks/123/abcdefSECRETtoken \
                     hex 0123456789abcdef0123456789abcdef0123";
        let out = redact_secrets(input);
        assert!(!out.contains("sk-ABCDEFGHIJKLMNOP"), "sk leaked: {out}");
        assert!(!out.contains("ghp_0123456789abcdefghij"), "ghp leaked: {out}");
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "akia leaked: {out}");
        assert!(!out.contains("myreallylongtoken123456"), "bearer leaked: {out}");
        assert!(!out.contains("supersecretvalue"), "kv leaked: {out}");
        assert!(!out.contains("abcdefSECRETtoken"), "webhook token leaked: {out}");
        assert!(out.contains("[REDACTED]"));
        // benign words preserved
        assert!(out.contains("key"));
        assert!(out.contains("Authorization:"));
    }

    #[test]
    fn test_redact_secrets_kv_value_in_next_token() {
        let out = redact_secrets("\"token\": \"abcdefghijklmnopqrstuvwx\"");
        assert!(!out.contains("abcdefghijklmnopqrstuvwx"), "value leaked: {out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn test_redact_secrets_idempotent_and_keeps_plain_text() {
        let plain = "hello world exit=0 done";
        assert_eq!(redact_secrets(plain), plain);
        let once = redact_secrets("API_KEY=supersecretvalue");
        let twice = redact_secrets(&once);
        assert_eq!(once, twice);
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
        assert!(!out.contains("omitted"), "must not insert omission marker: {out}");
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
        assert!(m.contains("API_KEY=supersecretvalue"), "kv secret stripped: {m}");
        assert!(m.contains("ghp_0123456789abcdefghij"), "prefix secret stripped: {m}");
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
        assert!(msgs.len() > 1, "long output must be split into multiple parts");
        // every part is within Discord's hard limit and labelled in order.
        for (i, m) in msgs.iter().enumerate() {
            assert!(m.chars().count() <= DISCORD_MESSAGE_LIMIT, "part too long: {}", m.len());
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
        assert!(reconstructed.contains(&stdout), "reconstruction lost stdout");
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
        assert!(joined.contains("partial output"), "must mark partial: {joined}");
    }

    // ---- activity-family resolution ----

    #[test]
    fn test_resolve_activity_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        insert_row(&conn, "tool", "a1", "execute_shell", "activity", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "activity", VALID_URL, true);
        let r2 = resolve_activity_webhook(&conn2, "a1", "execute_shell");
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "activity", VALID_URL, true);
        let r3 = resolve_activity_webhook(&conn3, "a1", "execute_shell");
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_resolve_activity_ignores_subtask_kind_and_has_no_env() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a subtask-kind agent row exists -> activity resolution must NOT use it.
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(matches!(r, WebhookResolution::None), "subtask kind must not serve activity");
    }

    #[test]
    fn test_resolve_activity_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "tool", "a1", "execute_shell", "activity", VALID_URL, false);
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(matches!(r, WebhookResolution::Disabled { source: WebhookSource::ToolDefault }));
    }

    #[test]
    fn test_resolve_activity_invalid_db_default_errors() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", "http://bad", true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::AgentDefault);
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_resolve_activity_also_serves_subtask_lifecycle() {
        // An agent 'activity' default should also be picked up by resolve_subtask_webhook
        // (activity family includes subtask lifecycle).
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_resolve_subtask_prefers_explicit_subtask_over_activity_same_scope() {
        // L3: 同一 scope に subtask 専用行と汎用 activity 行が両方あるとき、subtask 通知は
        // 明示的な subtask 専用デフォルトへ送る（activity に奪われない）。
        const SUBTASK_URL: &str = "https://discord.com/api/webhooks/111/subtasktoken";
        const ACTIVITY_URL: &str = "https://discord.com/api/webhooks/222/activitytoken";
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", ACTIVITY_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", SUBTASK_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(
                    config.url, SUBTASK_URL,
                    "subtask-specific default must win over generic activity"
                );
            }
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_resolve_subtask_falls_back_to_activity_when_no_subtask_row() {
        // subtask 専用行が無ければ activity 行へフォールバックする（family 包含）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    // ---- delivery failure recording ----

    #[test]
    fn test_record_webhook_delivery_failure_writes_redacted_log() {
        let conn = opencrab_db::init_memory().unwrap();

        let redacted = redact_webhook_url(VALID_URL);
        record_webhook_delivery_failure(
            &conn,
            "a1",
            "parent-sess",
            "st1",
            "subtask-st1",
            &redacted,
            "http 500",
        );

        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, "parent-sess").unwrap();
        let found = logs
            .iter()
            .find(|l| l.content.contains("delivery_failed"))
            .expect("delivery_failed log should exist");
        assert!(found.content.contains("[redacted]"));
        assert!(
            !found.content.contains(SECRET_TOKEN),
            "raw token leaked into log: {}",
            found.content
        );

        // empty parent_session_id -> no-op
        record_webhook_delivery_failure(&conn, "a1", "", "st1", "s", &redacted, "x");
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
        let resolved = resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &empty_args, None);
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
        eprintln!("[e2e] delivered {} message(s) to the default webhook", messages.len());

        // (2) 非空の不正 url はフォールバックせず Error（strict 維持）。
        let bad_args = json!({ "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" } });
        let bad = resolve_subtask_webhook(&conn, &agent_id, "spawn_subtask", &bad_args, None);
        match bad {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
                assert!(!message.contains("evil.example.com"), "raw url leaked: {message}");
                eprintln!("[e2e] invalid explicit url -> Error (no fallback): {code}: {message}");
            }
            _ => panic!("non-empty invalid explicit url must Error, not fall back"),
        }
    }
}
