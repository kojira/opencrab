//! サブタスクエンジン操作（Discord に残る分: 完了 sink と activity sink）。
//!
//! `spawn_subtask` / `report_progress` は gateway 非依存層へ移設済み（#175 S4:
//! `crates/server/src/subtask_spawn.rs` と `crates/server/src/system_actions.rs`）。
//! `cancel_subtask` も #157 S2 / #184 で移設済み（`opencrab_actions::cancel_subtask` が
//! 唯一の実装。sub-session theme からの説明文解決と lifecycle 通知もそちらへ移した）。
//! ここに残るのは Discord 固有の 2 つだけ:
//! - `DiscordCompletionSink`（決着を Discord のイベントループへ再注入する）
//! - activity webhook 向けの `ToolEventSink` とその factory

use std::sync::Arc;

use super::webhook::{self, DeliveryBatch, WebhookResolution};
use crate::message_loop::{parse_discord_session, LoopEvent};
use opencrab_actions::subtask::{SubtaskCompletionSink, SubtaskSettled};

/// `SubtaskCompletionSink` の Discord 実装（RFC #152 S1）。
///
/// 旧 `send_subtask_completed_event`（LoopEvent 直依存）を置換する。runtime
/// （actions 側 `settle_completed` / progress debounce）は `Arc<dyn
/// SubtaskCompletionSink>` としてこれを呼ぶだけで、`LoopEvent` を知らない。
/// `parse_discord_session` / `LoopEvent` は Discord に閉じたままここに残す。
///
/// parent_session_id から routing 情報を復元して `LoopEvent::SubtaskCompleted` を送る
/// （#39）。session_id（`discord-{agent}-{guild}-{channel}`）から導出できるため、
/// クロージャの登録は不要。event_tx 未設定（イベントループの無い構築、例: 一発呼びの
/// API 経路）や Discord 形式でない session は、旧実装で未登録だった場合と同様に発火
/// しない（debug のみ）。
///
/// **web / Nostr sink との意図的な差分**: あちらは `kind != SettleKind::Completed` を
/// 捨てるが、Discord は `Progress` も送る。`report_progress` のデバウンス発火が
/// この sink を通ってメインエンジンを呼び直す「進捗実況」機能で、main の
/// `send_subtask_completed_event(..., "progress")` から続く既存挙動だから
/// （ガードを足すと機能が黙って消える）。`Cancelled` は別メソッド
/// （`on_subtask_cancelled` の既定実装 = 何もしない）なのでここには来ない。
/// この差分は `discord_sink_forwards_progress_unlike_web_and_nostr` で固定している。
pub(crate) struct DiscordCompletionSink {
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
}

impl SubtaskCompletionSink for DiscordCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        let Some(tx) = &self.event_tx else {
            tracing::debug!(
                session_id = %ev.session_id,
                "subtask completion: event_tx not configured, skipping main-engine notification"
            );
            return;
        };
        let Some((guild_id, channel_id)) = parse_discord_session(&ev.session_id) else {
            // 非 Discord の親セッション（heartbeat-* / subtask-* のネスト等）は正常系。
            // 旧レジストリ実装でも未登録で発火しなかったため、debug に留める。
            tracing::debug!(
                session_id = %ev.session_id,
                "subtask completion: parent session is not a discord session, skipping main-engine notification"
            );
            return;
        };
        let is_dm = guild_id.is_empty();
        let _ = tx.send(LoopEvent::SubtaskCompleted {
            session_id: ev.session_id,
            agent_id: ev.agent_id,
            subtask_id: ev.subtask_id,
            // 本文は運ばない。完了本文は DB（session_logs）へ永続化済みで、再注入は
            // `build_conversation_string` が DB から読み直す（`process_subtask_completed`
            // の引数は `_result` = 未使用。RFC §1.3）。
            result: String::new(),
            exit_reason: ev.exit_reason,
            channel_id,
            channel_id_str: channel_id.to_string(),
            guild_id,
            is_dm,
        });
    }
}

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
                )],
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
mod tests {
    use super::*;
    use opencrab_actions::subtask::SettleKind;

    // ---- RFC #152 S1: DiscordCompletionSink（完了の再注入経路） ----
    //
    // この sink は「dispatch した全ツールの結果を親会話へ戻す」唯一の口なので、
    // 空実装にしても他テストが緑のままだと退行を検知できない（#165 レビュー P1。
    // web sink に対する同種の指摘と同じ）。実 mpsc を張って、送出の有無と
    // routing 復元内容をここで直接固定する。

    /// テスト用: 実チャネルを張った sink と受信側を作る。
    fn sink_with_channel() -> (
        DiscordCompletionSink,
        tokio::sync::mpsc::UnboundedReceiver<LoopEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (DiscordCompletionSink { event_tx: Some(tx) }, rx)
    }

    fn settled(session_id: &str, kind: SettleKind, exit_reason: &str) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-x".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: exit_reason.to_string(),
            kind,
            reply_target: None,
        }
    }

    /// 完了 → `LoopEvent::SubtaskCompleted` がちょうど 1 本流れ、guild/channel が
    /// parent_session_id から復元される（本文は運ばない = `result` は空）。
    #[test]
    fn discord_sink_emits_loop_event_on_completion() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Completed,
            "completed",
        ));

        match rx.try_recv().expect("完了は LoopEvent を 1 本送る") {
            LoopEvent::SubtaskCompleted {
                session_id,
                agent_id,
                subtask_id,
                result,
                exit_reason,
                channel_id,
                channel_id_str,
                guild_id,
                is_dm,
            } => {
                assert_eq!(session_id, "discord-agent-x-111222333-444555666");
                assert_eq!(agent_id, "agent-x");
                assert_eq!(subtask_id, "st-1");
                // 本文は DB（session_logs）から読み直す契約（RFC §1.3）。
                assert_eq!(result, "");
                assert_eq!(exit_reason, "completed");
                assert_eq!(channel_id, 444_555_666);
                assert_eq!(channel_id_str, "444555666");
                assert_eq!(guild_id, "111222333");
                assert!(!is_dm);
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err(), "余分なイベントを送ってはならない");
    }

    /// DM（guild_id 空）の親セッションでも復元でき、`is_dm` が立つ。
    #[test]
    fn discord_sink_restores_dm_routing() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x--444555666",
            SettleKind::Completed,
            "timeout",
        ));

        match rx.try_recv().expect("DM でも LoopEvent を送る") {
            LoopEvent::SubtaskCompleted {
                guild_id,
                channel_id,
                is_dm,
                exit_reason,
                ..
            } => {
                assert_eq!(guild_id, "");
                assert_eq!(channel_id, 444_555_666);
                assert!(is_dm, "guild_id が空なら DM 扱い");
                // exit_reason は完了理由をそのまま運ぶ（completed 以外も再注入する）。
                assert_eq!(exit_reason, "timeout");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err());
    }

    /// 非 Discord の親セッション（web / heartbeat / nostr / ネストした subtask）は
    /// 正常系としてスキップする。各 gateway の sink が自分のセッションだけを拾う。
    #[test]
    fn discord_sink_skips_non_discord_sessions() {
        for session_id in [
            "web-agent-x-conv-1",
            "heartbeat-agent-x",
            "nostr-agent-x-npub1abc",
            "subtask-11111111-2222-3333-4444-555555555555",
            "agent-msg-agent-x-user-1",
            "",
        ] {
            let (sink, mut rx) = sink_with_channel();
            sink.on_subtask_settled(settled(session_id, SettleKind::Completed, "completed"));
            assert!(
                rx.try_recv().is_err(),
                "非 Discord セッション '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// Discord 形式に見えて壊れている session_id（channel が数値でない等）も送らない。
    #[test]
    fn discord_sink_skips_malformed_discord_sessions() {
        for session_id in [
            "discord-agent-x-111-notanumber",
            "discord-agent-x-notanumber-444",
            "discord--111-444",
            "discord-agent-x",
        ] {
            let (sink, mut rx) = sink_with_channel();
            sink.on_subtask_settled(settled(session_id, SettleKind::Completed, "completed"));
            assert!(
                rx.try_recv().is_err(),
                "壊れた session_id '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// **意図的な差分**: Discord は `SettleKind::Progress` でも LoopEvent を送る
    /// （web / Nostr の sink は Completed 以外を捨てる）。
    ///
    /// `report_progress` のデバウンス発火はこの sink を通ってメインエンジンを
    /// 呼び直す実況機能で、main の `send_subtask_completed_event(..., "progress")`
    /// から続く既存挙動。ここで捨てると進捗実況が黙って消えるため、
    /// web / Nostr と同じ `kind != Completed` ガードは**入れない**。
    /// 差分を退行ではなく仕様として固定するためのテスト。
    #[test]
    fn discord_sink_forwards_progress_unlike_web_and_nostr() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Progress,
            "progress",
        ));

        match rx.try_recv().expect("進捗もメインエンジンへ再注入する") {
            LoopEvent::SubtaskCompleted { exit_reason, .. } => {
                assert_eq!(exit_reason, "progress");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
    }

    /// `on_subtask_cancelled` は既定実装（debug ログのみ）のまま = 停止では
    /// 再注入しない（止めたのに返信が届くのを防ぐ）。
    #[test]
    fn discord_sink_does_not_reinject_on_cancel() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_cancelled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Cancelled,
            "cancelled",
        ));
        assert!(
            rx.try_recv().is_err(),
            "cancel で LoopEvent を送ってはならない"
        );
    }

    /// event_tx 未設定（イベントループの無い構築）は no-op で panic しない。
    #[test]
    fn discord_sink_without_event_tx_is_noop() {
        let sink = DiscordCompletionSink { event_tx: None };
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Completed,
            "completed",
        ));
    }

    fn insert_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
            events_json: None,
            enabled: true,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    #[test]
    fn test_webhook_tool_event_sink_preserves_shell_output_unredacted() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({
            "exit_code": 0,
            "stdout": "leaked API_KEY=supersecretvalue here",
            "truncated": false
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("a batch should be sent");
        let msg = batch.messages.join("");
        assert!(msg.contains("tool_call_completed"));
        assert!(msg.contains("exit_code"));
        // covered 経路: stdout の secret はそのまま届く（masking しない）。
        assert!(
            msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {msg}"
        );
        assert!(!msg.contains("[REDACTED]"), "masking marker present: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_sends_failed_and_rejected() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "denied" });
        let failed_result = serde_json::json!({
            "exit_code": 2,
            "stderr": "API_KEY=supersecretvalue failed",
            "truncated": false
        });
        let failed = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "failed-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Failed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&failed_result),
            error: Some("command failed"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &failed);
        let rejected = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "rejected-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Rejected,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(1),
            args: &args,
            result: None,
            error: Some("permission denied"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &rejected);

        let failed_batch = rx.try_recv().expect("failed batch");
        let failed_msg = failed_batch.messages.join("");
        assert!(failed_msg.contains("tool_call_failed"));
        assert!(failed_msg.contains("exit_code"));
        // covered 経路: stderr の secret はそのまま届く（masking しない）。
        assert!(
            failed_msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {failed_msg}"
        );
        assert!(
            !failed_msg.contains("[REDACTED]"),
            "masking marker present: {failed_msg}"
        );
        let rejected_batch = rx.try_recv().expect("rejected batch");
        let rejected_msg = &rejected_batch.messages[0];
        assert!(rejected_msg.contains("tool_call_rejected"));
        assert!(rejected_msg.contains("permission denied"));
    }

    #[test]
    fn test_webhook_tool_event_sink_no_activity_row_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
        let args = serde_json::json!({});
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        assert!(rx.try_recv().is_err(), "no activity row -> nothing sent");
    }

    fn insert_activity_row(conn: &rusqlite::Connection, url: &str, enabled: bool) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
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

    fn make_sink(
        db: opencrab_db::Db,
        tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    ) -> WebhookToolEventSink {
        WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        }
    }

    fn started_event<'a>(args: &'a serde_json::Value) -> opencrab_actions::ToolEvent<'a> {
        opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args,
            result: None,
            error: None,
        }
    }

    // ---- tool/command argument inclusion on activity webhook messages ----

    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_args() {
        // started イベントにコマンド引数が含まれること（depth0 を想定）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "git status --short" });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("args:"), "args line missing: {msg}");
        assert!(msg.contains("git status --short"), "command missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_and_args_array() {
        // E2E 再現: command `echo` と args `["hello","webhook-args-test"]` が
        // started イベントで両方描画されること（args 配列が欠落しない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("echo"), "command missing: {msg}");
        assert!(msg.contains("hello"), "first arg missing: {msg}");
        assert!(
            msg.contains("webhook-args-test"),
            "second arg missing: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_non_shell_args() {
        // 非 shell ツールでも started に引数（JSON）が含まれる。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "path": "notes/todo.md", "limit": 10 });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "read_file",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("notes/todo.md"), "args missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_preserves_secret_args_unredacted() {
        // covered 経路: started の引数に含まれるシークレット（API キー / Discord webhook
        // URL）も masking せずそのまま届く（新要件 §2 P4 / AC4）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer sk-supersecretkeyvalue1234' https://discord.com/api/webhooks/999/leakedtokenvalue && export API_KEY=anothersupersecretvalue"
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = batch.messages.join("");
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        // every secret-like token survives unmodified, and no masking markers appear.
        assert!(
            !msg.contains("[REDACTED]"),
            "REDACTED marker present: {msg}"
        );
        assert!(
            !msg.contains("[redacted]"),
            "redacted marker present: {msg}"
        );
        assert!(
            msg.contains("sk-supersecretkeyvalue1234"),
            "api key stripped: {msg}"
        );
        assert!(
            msg.contains("https://discord.com/api/webhooks/999/leakedtokenvalue"),
            "webhook url stripped: {msg}"
        );
        assert!(
            msg.contains("API_KEY=anothersupersecretvalue"),
            "API_KEY value stripped: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_long_args_chunked_losslessly() {
        // 長大な引数はクランプ（…）せず、Discord 上限内の part X/N へロスレス分割する。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let long_cmd = format!("echo {}", "word ".repeat(2_000));
        let args = serde_json::json!({ "command": long_cmd });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        assert!(batch.messages.len() > 1, "long args must split into parts");
        // each part within Discord hard limit, labelled in order.
        for (i, m) in batch.messages.iter().enumerate() {
            assert!(
                m.chars().count() <= 2000,
                "part exceeds limit: {}",
                m.chars().count()
            );
            assert!(
                m.starts_with(&format!("part {}/{}\n", i + 1, batch.messages.len())),
                "part marker/order wrong: {m}"
            );
        }
        // reconstruct -> all 2000 'word' tokens present, no ellipsis loss.
        let reconstructed: String = batch
            .messages
            .iter()
            .map(|m| m.splitn(2, '\n').nth(1).unwrap_or("").to_string())
            .collect();
        assert!(!reconstructed.contains('…'), "clamp ellipsis introduced");
        assert_eq!(reconstructed.matches("word").count(), 2_000, "lost args");
    }

    // ---- summarize_tool_args: unredacted, lossless ----

    #[test]
    fn test_summarize_tool_args_preserves_secrets_unredacted() {
        // covered 経路: 引数中の secret も masking/クランプせずそのまま残す。
        let secret = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd"; // 40 文字の英数字
        let prefix = "a ".repeat(145); // 290 文字
        let cmd = format!("{prefix}{secret}");
        let args = serde_json::json!({ "command": cmd });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.starts_with("cmd: `"), "summary: {summary}");
        assert!(summary.contains(secret), "secret stripped: {summary}");
        assert!(
            !summary.contains("[REDACTED]"),
            "masking marker present: {summary}"
        );
        assert!(
            !summary.contains('…'),
            "clamp ellipsis introduced: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_preserves_webhook_url() {
        // /api/webhooks/ を含む URL がそのまま残ること（バイト一致）。
        let url =
            "https://discord.com/api/webhooks/123456789012345678/AbCdEf-XXXXXXXXXXXXXXXXXXXXXXXX";
        let args = serde_json::json!({ "command": format!("curl {url}") });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains(url), "webhook url stripped: {summary}");
        assert!(
            !summary.contains("[redacted]"),
            "url masking present: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_includes_command_and_args() {
        // execute_shell の実引数（command + args 配列）が両方描画されること。
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("echo"), "command missing: {summary}");
        assert!(summary.contains("hello"), "first arg missing: {summary}");
        assert!(
            summary.contains("webhook-args-test"),
            "second arg missing: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_marks_stdin_without_leaking() {
        // stdin は本文を出さず、存在とバイト数のみ示す。
        let args = serde_json::json!({
            "command": "cat",
            "stdin": "secret-stdin-body"
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("cat"), "command missing: {summary}");
        assert!(summary.contains("stdin"), "stdin marker missing: {summary}");
        assert!(
            !summary.contains("secret-stdin-body"),
            "stdin body leaked: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_empty_is_none() {
        assert!(summarize_tool_args("read_file", &serde_json::json!({})).is_none());
        assert!(summarize_tool_args("read_file", &serde_json::Value::Null).is_none());
    }

    // ---- L1: disabled/invalid activity row drops events (no silent fallback) ----

    #[test]
    fn test_webhook_tool_event_sink_disabled_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", false);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(rx.try_recv().is_err(), "disabled activity -> nothing sent");
    }

    #[test]
    fn test_webhook_tool_event_sink_invalid_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        // invalid (non-discord) url -> WebhookResolution::Error, must drop, no fallback.
        insert_activity_row(&conn, "https://evil.example.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(
            rx.try_recv().is_err(),
            "invalid activity url -> nothing sent"
        );
    }

    // ---- L2: shared delivery path preserves lifecycle/tool_call ordering ----

    #[test]
    fn test_shared_worker_channel_preserves_order() {
        // 単一の共有 tx を使うと、先に送った lifecycle batch のあとに tool_call event が
        // 続き、FIFO 順序が保たれる（別 worker だと順序保証が崩れる）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();

        // lifecycle 相当の batch を共有 tx へ先に送る。
        tx.send(DeliveryBatch {
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
            messages: vec!["lifecycle: started".to_string()],
        })
        .unwrap();

        // 同じ tx を使う sink から tool_call event を送る。
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({ "exit_code": 0, "stdout": "ok", "truncated": false });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "t",
            duration_ms: Some(1),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);

        // 受信順: lifecycle が先、tool_call が後。
        let first = rx.try_recv().expect("lifecycle batch");
        assert!(first.messages[0].contains("lifecycle: started"));
        let second = rx.try_recv().expect("tool_call batch");
        assert!(second.messages[0].contains("tool_call_completed"));
    }

    #[test]
    fn test_activity_diagnostic_batch_for_invalid_explicit_webhook_url() {
        // 非空の不正 explicit url は resolution Error を生み、その診断が activity default
        // へ redacted で配送されることを担保する。空 url はもはや Error にならない
        // （default へフォールバックする）ため、ここでは非空の不正 url を使う。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let args = serde_json::json!({
            "task": "do it",
            "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" }
        });
        let batch = build_activity_diagnostic_batch(
            &db,
            "a1",
            "spawn_subtask",
            "webhook_resolution_error",
            "spawn_subtask webhook resolution failed before execution: invalid_webhook_url: url must start with https:// (source: explicit)",
            &args,
        )
        .expect("diagnostic should route to activity default");
        assert_eq!(batch.url, "https://discord.com/api/webhooks/1/tok");
        let msg = &batch.messages[0];
        assert!(msg.contains("webhook_resolution_error"));
        assert!(msg.contains("invalid_webhook_url"));
        assert!(msg.contains("source: explicit"));
        assert!(!msg.contains("https://discord.com/api/webhooks/1/tok"));
    }

    // ---- depth0/main executor sink wiring (factory) ----

    /// activity 行が無いエージェントでは factory は None を返す（worker も起動しない）。
    #[tokio::test]
    async fn test_spawn_activity_sink_none_without_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_none(), "no activity row -> no sink");
    }

    /// activity 行があれば factory は Some を返し、その sink は depth0 イベントを
    /// activity webhook へ整形して配送する（covered 経路ゆえ unredacted で配送する）。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_activity_row_and_delivers() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_some(), "activity row -> sink present");

        // depth0 のツールイベントを流すと配送される（worker が実際に送ろうとするが、
        // ダミー URL なのでネットワークは best-effort で失敗する。ここでは on_event が
        // パニックせず整形できることを確認する）。
        let sink = sink.unwrap();
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({
            "exit_code": 0,
            "stdout": "leaked API_KEY=supersecretvalue here",
            "truncated": false
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        sink.on_event(&ev);
    }

    fn insert_global_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "global".to_string(),
            agent_id: "*".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/9/glob".to_string(),
            events_json: None,
            enabled: true,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    /// global(`*`) のみの activity デフォルトでも factory は Some を返す
    /// （list_agent_webhook_config が agent_id='*' を含むため）。depth0 イベントが
    /// global 宛先へ stream され得ることを担保する。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_global_only_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_global_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        // agent "a1" 固有の行は無いが、global 行があるので Some。
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(
            sink.is_some(),
            "global-only activity default -> sink present"
        );
    }
}
