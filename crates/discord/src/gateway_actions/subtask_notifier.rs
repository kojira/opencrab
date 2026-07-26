//! サブタスク lifecycle 通知の Discord 実装（#175 S3）。
//!
//! `opencrab_actions::subtask_notify` の抽象に対する Discord webhook 実装。中身は
//! 従来 `execute_spawn_subtask` / `execute_report_progress` / `execute_cancel_subtask`
//! の本体に埋まっていた webhook 呼び出しを**そのまま**呼ぶだけで、整形・配送・リトライ・
//! 解決順序は `super::webhook` に据え置く（1 行も変更していない）。
//!
//! ここに寄せたもの:
//! - 宛先の解決（`resolve_subtask_webhook`: explicit > tool > agent > global > env）
//! - 配送ワーカーの起動（lifecycle と tool_call_* を 1 本の worker で直列化する）
//! - 配送を諦めたときの親セッションログ記録（give-up sink）
//! - 解決エラー / 非 ok 状態の診断イベント送出
//! - 開始 / 進捗 / 終了 / 中断メッセージの組み立てと送出
//!
//! 呼び出し側（`subtask_engine`）は `Arc<dyn SubtaskRunNotifier>` だけを持ち、
//! `WebhookConfig` も `DeliveryBatch` も見ない。

use std::sync::Arc;

use opencrab_actions::subtask_notify::{
    NotifyTarget, NotifyTargetError, SubtaskLifecycleNotifier, SubtaskNotifySession,
    SubtaskRunInfo, SubtaskRunNotifier,
};
use opencrab_actions::ToolEventSink;

use super::subtask_engine::{emit_activity_diagnostic, WebhookToolEventSink};
use super::webhook::{
    self, DeliveryBatch, LifecycleMeta, WebhookConfig, WebhookResolution, WebhookSource,
};

/// activity ツールイベントの 1 run あたり送出上限と本文長（従来値をそのまま保つ）。
const TOOL_EVENT_MAX_CHARS: usize = 1500;
const TOOL_EVENT_CAP: usize = 200;

/// subtask lifecycle を Discord webhook へ通知するファクトリ。
///
/// 走行ごとに宛先を解決し、その run 専用の配送ワーカーを 1 本起動して
/// [`DiscordWebhookRunNotifier`] を返す。
pub struct DiscordWebhookNotifier {
    db: opencrab_db::Db,
    /// 配送ワーカーが共有する HTTP クライアント。
    client: reqwest::Client,
    /// ツール引数で明示指定が無いときの既定 lifecycle webhook（env/config 由来）。
    default_subtask_webhook: Option<WebhookConfig>,
}

impl DiscordWebhookNotifier {
    /// 配送用の HTTP クライアントは内部で 1 つ作る（呼び出し側 = server crate に
    /// reqwest 依存を持ち込まないため）。
    pub fn new(db: opencrab_db::Db, default_subtask_webhook: Option<WebhookConfig>) -> Self {
        Self {
            db,
            client: reqwest::Client::new(),
            default_subtask_webhook,
        }
    }
}

impl SubtaskLifecycleNotifier for DiscordWebhookNotifier {
    fn begin_run(
        &self,
        run: &SubtaskRunInfo<'_>,
    ) -> Result<SubtaskNotifySession, NotifyTargetError> {
        // Subtask lifecycle webhook を固定順序で解決する（explicit > tool > agent > global > env）。
        // db lock は解決の間だけ握り、await をまたがない。
        let resolution = {
            let conn = self.db.lock().unwrap();
            webhook::resolve_subtask_webhook(
                &conn,
                run.agent_id,
                "spawn_subtask",
                run.tool_args,
                self.default_subtask_webhook.as_ref(),
            )
        };

        // 解決結果を webhook 設定 + 可視性メタへ写像する。
        let (webhook, webhook_source, webhook_status): (
            Option<WebhookConfig>,
            Option<WebhookSource>,
            &'static str,
        ) = match resolution {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                emit_activity_diagnostic(
                    self.db.clone(),
                    self.client.clone(),
                    run.agent_id,
                    "spawn_subtask",
                    "webhook_resolution_error",
                    &format!(
                        "spawn_subtask webhook resolution failed before execution: {code}: {message} (source: {})",
                        source.as_str()
                    ),
                    run.tool_args,
                    None,
                );
                // 検証失敗 → spawn しない。raw url はどこにも出さない。
                return Err(NotifyTargetError {
                    code,
                    message,
                    source: source.as_str(),
                });
            }
            WebhookResolution::Use { config, source } => (Some(config), Some(source), "ok"),
            WebhookResolution::Disabled { source } => (None, Some(source), "disabled"),
            WebhookResolution::None => (None, None, "none"),
        };
        let webhook_redacted_url = webhook
            .as_ref()
            .map(|cfg| webhook::redact_webhook_url(&cfg.url));
        let webhook_source_str: Option<&'static str> = webhook_source.map(|s| s.as_str());

        // give-up 時に親セッションログへ 1 件記録する sink を構築する。
        let giveup_sink: Option<Arc<dyn Fn(&str) + Send + Sync>> =
            if webhook.is_some() && !run.parent_session_id.is_empty() {
                let db_sink = self.db.clone();
                let agent_sink = run.agent_id.to_string();
                let parent_sink = run.parent_session_id.to_string();
                let subtask_sink = run.subtask_id.to_string();
                let sub_session_sink = run.sub_session_id.to_string();
                let redacted_sink = webhook_redacted_url.clone().unwrap_or_default();
                Some(Arc::new(move |error: &str| {
                    if let Ok(conn) = db_sink.lock() {
                        webhook::record_webhook_delivery_failure(
                            &conn,
                            &agent_sink,
                            &parent_sink,
                            &subtask_sink,
                            &sub_session_sink,
                            &redacted_sink,
                            error,
                        );
                    }
                }))
            } else {
                None
            };

        // 一般ツール/コマンド活動（activity family）のデフォルト webhook があるか。
        // 判定は webhook::has_activity_default に集約（resolve_activity_webhook と同じ
        // scope 集合: tool/agent/global の enabled な activity 行。env/config fallback なし）。
        let has_activity = {
            let conn = self.db.lock().unwrap();
            webhook::has_activity_default(&conn, run.agent_id)
        };

        // 同一 run の配送を直列化する worker を 1 つだけ起動する。lifecycle（started/
        // completed/...）と tool_call_*（activity）を同じ tx に流すことで、両系統の
        // 送出順序を 1 本の worker で保証する（別 worker を立てて順序が崩れるのを防ぐ）。
        let webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>> =
            if webhook.is_some() || has_activity {
                Some(webhook::spawn_run_worker_with_sink(
                    self.client.clone(),
                    giveup_sink,
                ))
            } else {
                None
            };

        if webhook_status != "ok" {
            emit_activity_diagnostic(
                self.db.clone(),
                self.client.clone(),
                run.agent_id,
                "spawn_subtask",
                "webhook_resolution_diagnostic",
                &format!(
                    "spawn_subtask lifecycle webhook status is {webhook_status}; source={}",
                    webhook_source_str.unwrap_or("none")
                ),
                run.tool_args,
                webhook_tx.as_ref(),
            );
        }

        // activity webhook があれば、executor に挿す ToolEventSink を用意し
        // tool_call_* を共有 worker 経由で配送する。
        let tool_event_sink: Option<Arc<dyn ToolEventSink>> = match (has_activity, &webhook_tx) {
            (true, Some(tx)) => Some(Arc::new(WebhookToolEventSink::new(
                self.db.clone(),
                run.agent_id.to_string(),
                tx.clone(),
                TOOL_EVENT_MAX_CHARS,
                TOOL_EVENT_CAP,
            ))),
            _ => None,
        };

        let notifier = DiscordWebhookRunNotifier {
            webhook,
            webhook_tx,
            label: run.label.to_string(),
            subtask_id: run.subtask_id.to_string(),
            sub_session_id: run.sub_session_id.to_string(),
            tool_event_sink,
        };

        Ok(SubtaskNotifySession {
            notifier: Arc::new(notifier),
            target: NotifyTarget {
                source: webhook_source_str,
                status: webhook_status,
                redacted_url: webhook_redacted_url,
            },
        })
    }
}

/// 1 走行ぶんの Discord webhook 通知口。
///
/// 送出可否は `WebhookConfig::wants`（購読イベントのフィルタ）に従い、本文の組み立ては
/// `webhook::build_*` をそのまま呼ぶ。よって送出内容は trait 導入前とバイト単位で同一。
pub(crate) struct DiscordWebhookRunNotifier {
    /// lifecycle 通知の宛先。None なら lifecycle は送らない（activity のみのことがある）。
    webhook: Option<WebhookConfig>,
    /// 同一 run の delivery を直列化する sender。
    webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>,
    label: String,
    subtask_id: String,
    sub_session_id: String,
    tool_event_sink: Option<Arc<dyn ToolEventSink>>,
}

impl DiscordWebhookRunNotifier {
    /// 宛先と sender が揃っていて、そのイベントを購読しているときだけ `Some` を返す。
    fn target_for(
        &self,
        event: &str,
    ) -> Option<(
        &WebhookConfig,
        &tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    )> {
        match (&self.webhook, &self.webhook_tx) {
            (Some(cfg), Some(tx)) if cfg.wants(event) => Some((cfg, tx)),
            _ => None,
        }
    }

    /// テスト用: 任意の sender を差した通知口を組む（宛先解決と配送を挟まずに
    /// 送出内容だけを検証するため）。
    #[cfg(test)]
    pub(crate) fn for_test(
        webhook: Option<WebhookConfig>,
        webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>,
        label: &str,
        subtask_id: &str,
        sub_session_id: &str,
    ) -> Self {
        Self {
            webhook,
            webhook_tx,
            label: label.to_string(),
            subtask_id: subtask_id.to_string(),
            sub_session_id: sub_session_id.to_string(),
            tool_event_sink: None,
        }
    }
}

impl SubtaskRunNotifier for DiscordWebhookRunNotifier {
    fn on_started(&self, task: &str) {
        if let Some((cfg, tx)) = self.target_for("started") {
            let meta = LifecycleMeta {
                label: self.label.clone(),
                run_id: self.subtask_id.clone(),
                session_key: self.sub_session_id.clone(),
            };
            let messages =
                webhook::build_started_messages(&meta, task, webhook::DISCORD_CHUNK_LIMIT);
            let _ = tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages,
            });
        }
    }

    fn on_progress(&self, detail: &str) {
        if let Some((cfg, tx)) = self.target_for("progress") {
            let msg =
                webhook::build_progress_message(&self.subtask_id, &self.sub_session_id, detail);
            let _ = tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages: vec![msg],
            });
        }
    }

    fn on_finished(&self, exit_reason: &str, duration_ms: u64, result_text: &str) {
        let status = webhook::exit_reason_to_status(exit_reason);
        if let Some((cfg, tx)) = self.target_for(status) {
            let msg = webhook::build_terminal_message(
                status,
                &self.subtask_id,
                &self.sub_session_id,
                Some(duration_ms),
                result_text,
            );
            let _ = tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages: vec![msg],
            });
        }
    }

    fn on_cancelled(&self, duration_ms: u64) {
        // アボートで spawned closure は中断されるため terminal completed/failed は
        // 来ない → ここが唯一の終端。
        if let Some((cfg, tx)) = self.target_for("aborted") {
            let msg = webhook::build_terminal_message(
                "aborted",
                &self.subtask_id,
                &self.sub_session_id,
                Some(duration_ms),
                "cancelled by request",
            );
            let _ = tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages: vec![msg],
            });
        }
    }

    fn wants_progress(&self) -> bool {
        self.target_for("progress").is_some()
    }

    fn tool_event_sink(&self) -> Option<Arc<dyn ToolEventSink>> {
        self.tool_event_sink.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://discord.com/api/webhooks/1/tok";

    fn notifier_with_events(
        events: Option<Vec<&str>>,
    ) -> (
        DiscordWebhookRunNotifier,
        tokio::sync::mpsc::UnboundedReceiver<DeliveryBatch>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = WebhookConfig {
            url: URL.to_string(),
            events: events.map(|e| e.into_iter().map(|s| s.to_string()).collect()),
        };
        (
            DiscordWebhookRunNotifier::for_test(Some(cfg), Some(tx), "job", "st-1", "subtask-st-1"),
            rx,
        )
    }

    /// 開始通知は `webhook::build_started_messages` の出力をそのまま送る
    /// （trait を挟んでもバイト単位で同一）。
    #[test]
    fn started_matches_webhook_formatting_byte_for_byte() {
        let (n, mut rx) = notifier_with_events(None);
        n.on_started("do the thing");
        let batch = rx.try_recv().expect("開始通知が送られる");
        assert_eq!(batch.url, URL);
        let expected = webhook::build_started_messages(
            &LifecycleMeta {
                label: "job".to_string(),
                run_id: "st-1".to_string(),
                session_key: "subtask-st-1".to_string(),
            },
            "do the thing",
            webhook::DISCORD_CHUNK_LIMIT,
        );
        assert_eq!(batch.messages, expected);
    }

    /// 進捗通知は `webhook::build_progress_message` の出力そのもの。
    #[test]
    fn progress_matches_webhook_formatting_byte_for_byte() {
        let (n, mut rx) = notifier_with_events(None);
        n.on_progress("halfway");
        let batch = rx.try_recv().expect("進捗通知が送られる");
        assert_eq!(
            batch.messages,
            vec![webhook::build_progress_message(
                "st-1",
                "subtask-st-1",
                "halfway"
            )]
        );
    }

    /// 終了通知は exit_reason を webhook のステータス語彙へ写像して送る。
    #[test]
    fn finished_maps_exit_reason_to_status() {
        for (exit_reason, status) in [
            ("completed", "completed"),
            ("stopped_by_limit", "completed"),
            ("error", "failed"),
            ("timeout", "timed_out"),
        ] {
            let (n, mut rx) = notifier_with_events(None);
            n.on_finished(exit_reason, 1234, "result body");
            let batch = rx.try_recv().expect("終了通知が送られる");
            assert_eq!(
                batch.messages,
                vec![webhook::build_terminal_message(
                    status,
                    "st-1",
                    "subtask-st-1",
                    Some(1234),
                    "result body"
                )],
                "exit_reason={exit_reason}"
            );
        }
    }

    /// 中断通知は aborted ステータスで、本文は従来と同じ固定文言。
    #[test]
    fn cancelled_sends_aborted_terminal_message() {
        let (n, mut rx) = notifier_with_events(None);
        n.on_cancelled(99);
        let batch = rx.try_recv().expect("中断通知が送られる");
        assert_eq!(
            batch.messages,
            vec![webhook::build_terminal_message(
                "aborted",
                "st-1",
                "subtask-st-1",
                Some(99),
                "cancelled by request"
            )]
        );
    }

    /// 購読していないイベントは送らない（`wants` のフィルタが実装側に残っている）。
    #[test]
    fn unsubscribed_events_are_not_sent() {
        let (n, mut rx) = notifier_with_events(Some(vec!["completed"]));
        n.on_started("task");
        n.on_progress("detail");
        n.on_cancelled(1);
        assert!(rx.try_recv().is_err(), "購読外のイベントは送らない");
        assert!(!n.wants_progress(), "progress 未購読ならフックを挿さない");
        n.on_finished("completed", 5, "ok");
        assert!(rx.try_recv().is_ok(), "購読しているイベントは送る");
    }

    /// 宛先が無い（lifecycle webhook 未設定）通知口は何も送らず、進捗も購読しない。
    #[test]
    fn notifier_without_target_is_silent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let n = DiscordWebhookRunNotifier::for_test(None, Some(tx), "job", "st-1", "subtask-st-1");
        n.on_started("task");
        n.on_progress("detail");
        n.on_finished("completed", 1, "r");
        n.on_cancelled(1);
        assert!(rx.try_recv().is_err());
        assert!(!n.wants_progress());
        assert!(n.tool_event_sink().is_none());
    }

    /// 宛先が解決できない（設定なし）なら status は none で、通知口は無音。
    #[tokio::test]
    async fn begin_run_without_config_yields_none_target() {
        let db = opencrab_db::Db::memory().unwrap();
        let factory = DiscordWebhookNotifier::new(db, None);
        let args = serde_json::json!({"task": "t"});
        let session = factory
            .begin_run(&SubtaskRunInfo {
                agent_id: "a1",
                subtask_id: "st-1",
                sub_session_id: "subtask-st-1",
                parent_session_id: "discord-a1-1-2",
                label: "job",
                tool_args: &args,
            })
            .expect("設定が無いのは失敗ではない");
        assert_eq!(session.target, NotifyTarget::none());
        assert!(!session.notifier.wants_progress());
        assert!(session.notifier.tool_event_sink().is_none());
    }

    /// 明示指定が壊れていれば解決エラー（= subtask を起動させない）。
    /// 生 URL は境界を越えない。
    #[tokio::test]
    async fn begin_run_with_invalid_explicit_url_errors() {
        let db = opencrab_db::Db::memory().unwrap();
        let factory = DiscordWebhookNotifier::new(db, None);
        let args = serde_json::json!({
            "task": "t",
            "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" }
        });
        let Err(err) = factory.begin_run(&SubtaskRunInfo {
            agent_id: "a1",
            subtask_id: "st-1",
            sub_session_id: "subtask-st-1",
            parent_session_id: "discord-a1-1-2",
            label: "job",
            tool_args: &args,
        }) else {
            panic!("不正な明示 URL は解決エラーでなければならない");
        };
        assert_eq!(err.source, "explicit");
        assert!(!err.code.is_empty());
        assert!(
            !err.message.contains("evil.example.com/api/webhooks/1/tok"),
            "生の宛先を漏らしてはならない: {}",
            err.message
        );
    }

    /// 明示指定が有効なら status=ok / source=explicit で、伏字化した宛先を返す。
    #[tokio::test]
    async fn begin_run_with_explicit_url_yields_ok_target() {
        let db = opencrab_db::Db::memory().unwrap();
        let factory = DiscordWebhookNotifier::new(db, None);
        let args = serde_json::json!({
            "task": "t",
            "webhook": { "url": URL, "events": ["started"] }
        });
        let session = factory
            .begin_run(&SubtaskRunInfo {
                agent_id: "a1",
                subtask_id: "st-1",
                sub_session_id: "subtask-st-1",
                parent_session_id: "discord-a1-1-2",
                label: "job",
                tool_args: &args,
            })
            .expect("有効な明示 URL は解決できる");
        assert_eq!(session.target.status, "ok");
        assert_eq!(session.target.source, Some("explicit"));
        let redacted = session.target.redacted_url.expect("伏字化した宛先を返す");
        assert_eq!(redacted, webhook::redact_webhook_url(URL));
        assert_ne!(redacted, URL, "生の宛先を返してはならない");
    }
}
