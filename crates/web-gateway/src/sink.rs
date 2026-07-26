//! subtask 完了の受け口（[`SubtaskCompletionSink`] の web 実装）。
//!
//! `crates/discord` は完了通知を `LoopEvent` へ送ってイベントループに resume させるが、
//! web にはループが無いので、ここで直接 `tokio::spawn` して resume する。
//!
//! **このモジュールは応答生成の本体（`crate::respond` の `run_and_deliver`）へ到達できない**
//! （兄弟モジュールの private 項目）。resume は必ず直列化込みの
//! [`run_and_deliver_serialized`] を通る = ロック取得の忘れがコンパイル時に不可能。

use opencrab_actions::{CallerIdentity, SettleKind, SubtaskCompletionSink, SubtaskSettled};

use crate::gateway::WEB_SESSION_PREFIX;
use crate::respond::run_and_deliver_serialized;
use crate::runner::WebAgentRunner;

/// subtask が settle したとき、web セッションなら per-session ロック下で親を resume し、
/// 生成メッセージを SSE で配送する。非 web の親セッション（heartbeat-* / subtask-* の
/// ネスト等）は正常系としてスキップする（Discord 実装が非 discord をスキップするのと同じ）。
pub struct WebCompletionSink<R: WebAgentRunner> {
    runner: R,
}

impl<R: WebAgentRunner> Clone for WebCompletionSink<R> {
    fn clone(&self) -> Self {
        Self {
            runner: self.runner.clone(),
        }
    }
}

impl<R: WebAgentRunner> WebCompletionSink<R> {
    /// SSE チャンネルと直列化ロックは runner から引く（`WebAgentRunner::web_gateway`）。
    /// inbound と同じランタイムに到達することが直列化の前提なので、別の
    /// ランタイムを渡せる余地を作らない。
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

/// resume 時に system prompt へ足す完了マーカー。
///
/// 完了本文そのものは `settle_completed` が DB へ永続化済みで、会話の再構築で拾われる。
fn resume_prompt_suffix(subtask_id: &str, exit_reason: &str) -> String {
    format!("[subtask_completed: subtask_id={subtask_id}, exit_reason={exit_reason}]")
}

impl<R: WebAgentRunner> SubtaskCompletionSink for WebCompletionSink<R> {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        // 決着（Completed）以外（進捗通知など）で resume すると、まだ走っている run の
        // 途中で二重に応答してしまう。型の意図をここで実際に守る。
        if ev.kind != SettleKind::Completed {
            tracing::debug!(
                session_id = %ev.session_id,
                kind = ?ev.kind,
                "web sink: not a completion, skipping resume"
            );
            return;
        }
        if !ev.session_id.starts_with(WEB_SESSION_PREFIX) {
            tracing::debug!(
                session_id = %ev.session_id,
                "web sink: parent session is not a web session, skipping resume"
            );
            return;
        }
        let runner = self.runner.clone();
        // sink は同期関数。resume は非同期のため spawn する（Discord が LoopEvent を
        // mpsc へ送るのと役割は同じ。web はループが無いので直接 spawn する）。
        tokio::spawn(async move {
            let note = resume_prompt_suffix(&ev.subtask_id, &ev.exit_reason);
            run_and_deliver_serialized(
                &runner,
                &ev.agent_id,
                &ev.session_id,
                CallerIdentity::Agent,
                Some(&note),
                "subtask_resume",
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::gateway::web_session_id;
    use crate::testing::FakeRunner;

    fn settled(session_id: &str, kind: SettleKind) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "a".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind,
            reply_target: None,
        }
    }

    /// 完了なら resume して `kind:"subtask_resume"` を配送し、完了マーカーを注入する。
    #[tokio::test]
    async fn completion_resumes_and_publishes() {
        let runner = FakeRunner::new("終わりました");
        let sid = web_session_id("a", "resume");
        let mut rx = runner.web_gateway().subscribe(&sid);

        WebCompletionSink::new(runner.clone())
            .on_subtask_settled(settled(&sid, SettleKind::Completed));

        let payload = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("resume が配送されない")
            .unwrap();
        assert!(payload.contains("\"kind\":\"subtask_resume\""));
        assert!(payload.contains("終わりました"));
        let runs = runner.runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0]
            .system_prompt
            .contains("[subtask_completed: subtask_id=st-1, exit_reason=completed]"));
    }

    /// 進捗通知（Progress）では resume しない（走行中の run への二重応答の防止）。
    #[tokio::test]
    async fn progress_does_not_resume() {
        let runner = FakeRunner::new("x");
        let sid = web_session_id("a", "progress");
        let mut rx = runner.web_gateway().subscribe(&sid);

        WebCompletionSink::new(runner.clone())
            .on_subtask_settled(settled(&sid, SettleKind::Progress));

        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "進捗通知で resume してしまっている"
        );
        assert!(runner.runs().is_empty());
    }

    /// 非 web の親セッションは無視する（Nostr / heartbeat のネスト等）。
    #[tokio::test]
    async fn non_web_sessions_are_ignored() {
        let runner = FakeRunner::new("x");
        let sink = WebCompletionSink::new(runner.clone());
        sink.on_subtask_settled(settled("nostr-a-pk", SettleKind::Completed));
        sink.on_subtask_settled(settled("heartbeat-a", SettleKind::Completed));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(runner.runs().is_empty());
    }

    /// inbound の応答生成と resume が同一セッションで直列化される。
    #[tokio::test]
    async fn resume_serializes_with_inbound() {
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(120));
        let sid = web_session_id("a", "serial");

        let inbound = {
            let runner = runner.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                crate::run_and_deliver_serialized(
                    &runner,
                    "a",
                    &sid,
                    CallerIdentity::TrustedUser,
                    None,
                    "direct",
                )
                .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        WebCompletionSink::new(runner.clone())
            .on_subtask_settled(settled(&sid, SettleKind::Completed));

        inbound.await.unwrap();
        // resume 側の完走を待つ。
        for _ in 0..100 {
            if runner.runs().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(runner.runs().len(), 2);
        assert_eq!(
            runner.max_inflight(),
            1,
            "同一セッションの応答生成は同時に 1 本まで（二重回答の防止）"
        );
    }
}
