//! サブタスクエンジン操作（Discord に残る分: 完了 sink と activity sink）。
//!
//! `spawn_subtask` / `report_progress` は gateway 非依存層へ移設済み（#175 S4:
//! `crates/server/src/subtask_spawn.rs` と `crates/server/src/system_actions.rs`）。
//! `cancel_subtask` も #157 S2 / #184 で移設済み（`opencrab_actions::cancel_subtask` が
//! 唯一の実装。sub-session theme からの説明文解決と lifecycle 通知もそちらへ移した）。
//! ここに残るのは Discord 固有の 2 つだけ:
//! - `DiscordCompletionSink`（決着を Discord のイベントループへ再注入する）
//! - activity webhook 向けの `ToolEventSink` とその factory

use super::webhook::{self, DeliveryBatch, WebhookResolution};
use std::sync::Arc;

use std::sync::atomic::{AtomicUsize, Ordering};

/// depth0/メインエージェントの executor に挿す activity ツールイベント sink を構築する。
///
/// `agent_id` に対する有効な activity 行（agent scope または global `*`）が無ければ
/// `None` を返し、配送 worker も起動しない（best-effort・無駄なタスクを作らない）。
/// 返した sink は spawn_subtask の sub-engine 用 sink と同じ実体で、イベントごとに
/// `resolve_activity_webhook`（tool > agent > global）で宛先を解決し、
/// `build_tool_event_message` で整形（covered 経路ゆえ redaction せず、上限超過のみ
/// ロスレス chunk）してから送る。disabled/不正 URL は
/// 黙って下位へ fall through せず診断を残す（no-silent-fallback）。
///
/// メイン engine は spawn_subtask のような lifecycle webhook を持たないため、ここでは
/// 専用の run worker を 1 本だけ起動して tool_call_* を直列配送する。
pub fn spawn_activity_tool_event_sink(
    db: opencrab_db::Db,
    agent_id: &str,
) -> Option<Arc<dyn opencrab_actions::ToolEventSink>> {
    let has_activity = {
        let conn = db.lock().ok()?;
        webhook::has_activity_default(&conn, agent_id)
    };
    if !has_activity {
        return None;
    }
    let tx = webhook::spawn_run_worker_with_sink(reqwest::Client::new(), None);
    Some(Arc::new(WebhookToolEventSink {
        db,
        agent_id: agent_id.to_string(),
        tx,
        max_chars: 1500,
        counter: AtomicUsize::new(0),
        cap: 200,
    }))
}

// 診断イベント送出に要る独立した材料（DB / HTTP client / 宛先解決の各値 / 既存 tx）を
// 受け取るだけの関数。まとめる自然な単位が無いため許容する。
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_activity_diagnostic(
    db: opencrab_db::Db,
    client: reqwest::Client,
    agent_id: &str,
    tool_name: &str,
    diagnostic_event: &str,
    reason: &str,
    args: &serde_json::Value,
    existing_tx: Option<&tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>,
) {
    let Some(batch) =
        build_activity_diagnostic_batch(&db, agent_id, tool_name, diagnostic_event, reason, args)
    else {
        return;
    };
    if let Some(tx) = existing_tx {
        let _ = tx.send(batch);
    } else {
        let tx = webhook::spawn_run_worker_with_sink(client, None);
        let _ = tx.send(batch);
    }
}

fn build_activity_diagnostic_batch(
    db: &opencrab_db::Db,
    agent_id: &str,
    tool_name: &str,
    diagnostic_event: &str,
    reason: &str,
    args: &serde_json::Value,
) -> Option<DeliveryBatch> {
    let resolution = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "webhook_audit",
                    agent_id = %agent_id,
                    tool = %tool_name,
                    event = %diagnostic_event,
                    error = %e,
                    "activity webhook diagnostic could not lock db"
                );
                return None;
            }
        };
        webhook::resolve_activity_webhook(&conn, agent_id, tool_name)
    };
    let cfg = match resolution {
        WebhookResolution::Use { config, .. } => config,
        WebhookResolution::Error {
            code,
            message,
            source,
        } => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                source = %source.as_str(),
                code = %code,
                reason = %message,
                "activity webhook diagnostic dropped because default resolution failed"
            );
            return None;
        }
        WebhookResolution::Disabled { source } => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                source = %source.as_str(),
                "activity webhook diagnostic dropped because default is disabled"
            );
            return None;
        }
        WebhookResolution::None => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                "activity webhook diagnostic dropped because no default is configured"
            );
            return None;
        }
    };
    if !cfg.wants(diagnostic_event) && !cfg.wants("tool_call_failed") {
        return None;
    }
    let view = webhook::ToolEventView {
        event: diagnostic_event.to_string(),
        tool_name: tool_name.to_string(),
        tool_call_id: "diagnostic".to_string(),
        depth: 0,
        status: "failed".to_string(),
        args_summary: summarize_tool_args(tool_name, args),
        result_summary: Some(reason.to_string()),
        max_chars: 1500,
        ..Default::default()
    };
    Some(DeliveryBatch {
        url: cfg.url,
        messages: webhook::build_tool_event_message(&view),
    })
}

/// activity family のデフォルト webhook へ tool_call_* を配送する sink。
/// イベントごとに resolve_activity_webhook で宛先を解決（tool > agent > global）し、
/// build_tool_event_message で整形（covered 経路ゆえ unredacted、上限超過のみロスレス
/// chunk）してから送る。
pub(super) struct WebhookToolEventSink {
    db: opencrab_db::Db,
    agent_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    max_chars: usize,
    counter: AtomicUsize,
    cap: usize,
}

impl WebhookToolEventSink {
    pub(super) fn new(
        db: opencrab_db::Db,
        agent_id: String,
        tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
        max_chars: usize,
        cap: usize,
    ) -> Self {
        Self {
            db,
            agent_id,
            tx,
            max_chars,
            counter: AtomicUsize::new(0),
            cap,
        }
    }
}

impl opencrab_actions::ToolEventSink for WebhookToolEventSink {
    fn on_event(&self, ev: &opencrab_actions::ToolEvent<'_>) {
        use opencrab_actions::ToolEventStatus;
        let (event_name, status) = match ev.status {
            ToolEventStatus::Started => ("tool_call_started", "started"),
            ToolEventStatus::Completed => ("tool_call_completed", "completed"),
            ToolEventStatus::Failed => ("tool_call_failed", "failed"),
            ToolEventStatus::Rejected => ("tool_call_rejected", "rejected"),
        };
        let resolution = {
            let conn = match self.db.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            webhook::resolve_activity_webhook(&conn, &self.agent_id, ev.tool_name)
        };
        // Use 以外（Error/Disabled/None）はイベントを配送しない（no-silent-fallback）。
        // 黙って捨てると原因が見えないため、raw URL/token を載せずに診断を残す。
        let cfg = match resolution {
            WebhookResolution::Use { config, .. } => config,
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                tracing::warn!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    source = %source.as_str(),
                    code = %code,
                    reason = %message,
                    "activity webhook resolution error; tool event dropped"
                );
                return;
            }
            WebhookResolution::Disabled { source } => {
                tracing::debug!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    source = %source.as_str(),
                    "activity webhook disabled; tool event dropped"
                );
                return;
            }
            WebhookResolution::None => {
                tracing::trace!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    "no activity webhook configured for this tool; tool event dropped"
                );
                return;
            }
        };
        if !cfg.wants(event_name) {
            // events フィルタで落ちた場合も黙って捨てず、原因が追えるよう診断を残す
            // （raw URL/token は載せない）。canonical な status 名で一致判定している。
            tracing::debug!(
                target: "webhook_audit",
                agent_id = %self.agent_id,
                tool = %ev.tool_name,
                event = %event_name,
                "activity tool event filtered out by configured events list; tool event dropped"
            );
            return;
        }
        // per-run の暴走ガード（超過分は 1 通だけ抑制サマリ）。
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        if n == self.cap {
            let _ = self.tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages: vec![format!(
                    "(+ further tool events suppressed after {} this run)",
                    self.cap
                )
                .into()],
            });
            return;
        }
        if n > self.cap {
            return;
        }
        let mut view = webhook::ToolEventView {
            event: event_name.to_string(),
            tool_name: ev.tool_name.to_string(),
            tool_call_id: ev.tool_call_id.to_string(),
            depth: ev.depth,
            status: status.to_string(),
            duration_ms: ev.duration_ms,
            max_chars: self.max_chars,
            ..Default::default()
        };
        view.args_summary = summarize_tool_args(ev.tool_name, ev.args);
        match ev.status {
            ToolEventStatus::Completed | ToolEventStatus::Failed => {
                if ev.tool_name == "execute_shell" {
                    if let Some(data) = ev.result {
                        let s = webhook::summarize_shell_result(data);
                        view.exit_code = s.exit_code;
                        view.stdout_summary = s.stdout_summary;
                        view.stderr_summary = s.stderr_summary;
                        view.truncated = s.truncated;
                    }
                } else if let Some(e) = ev.error {
                    view.result_summary = Some(e.to_string());
                } else if let Some(data) = ev.result {
                    view.result_summary = Some(short_json_preview(data));
                }
            }
            ToolEventStatus::Rejected => {
                // 構造マーカー接頭辞は表示では落とし、人間可読の理由のみ残す。
                view.rejection_reason = ev.error.map(|s| {
                    s.strip_prefix(opencrab_actions::REJECTION_CODE_PREFIX)
                        .unwrap_or(s)
                        .to_string()
                });
            }
            ToolEventStatus::Started => {}
        }
        let messages = webhook::build_tool_event_message(&view);
        let _ = self.tx.send(DeliveryBatch {
            url: cfg.url.clone(),
            messages,
        });
    }
}

/// ツール引数の要約（execute_shell はコマンドを優先）。
///
/// covered 経路（work-channel 出力）のため redaction も length クランプも行わず、
/// command / args 配列をそのまま返す（docs/design-webhook-output-lossless.md §2 P4）。
/// Discord のサイズ上限は `build_tool_event_message` がロスレス chunk で吸収する。
fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name == "execute_shell" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            // command 単体ではなく、実際に渡された引数（args 配列）も含めて要約する。
            // これがないと `echo hello world` が `cmd: echo` としか表示されず欠落する。
            let mut parts = vec![format!("cmd: `{cmd}`")];
            if let Some(arr) = args.get("args").and_then(|v| v.as_array()) {
                let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !items.is_empty() {
                    // JSON 配列として描画する（例: ["hello","webhook-args-test"]）。
                    parts.push(format!("args: {}", serde_json::Value::from(items)));
                }
            }
            // stdin は本文を出さず、存在とバイト数のみ示す（出力ではなく入力の要約）。
            if let Some(stdin) = args.get("stdin").and_then(|v| v.as_str()) {
                if !stdin.is_empty() {
                    parts.push(format!("stdin: {} bytes", stdin.len()));
                }
            }
            return Some(parts.join(" "));
        }
    }
    let s = args.to_string();
    if s == "null" || s == "{}" {
        return None;
    }
    Some(s)
}

/// 非 shell ツールの result の preview（covered 経路: redact もクランプもしない）。
fn short_json_preview(data: &serde_json::Value) -> String {
    data.to_string()
}

#[cfg(test)]
#[path = "subtask_engine/tests/mod.rs"]
mod tests;
