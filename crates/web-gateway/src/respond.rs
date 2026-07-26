//! 応答生成 ＋ SSE 配送の共通経路（inbound と subtask 完了 resume の両方が使う）。
//!
//! **このモジュールの境界が #177 の構造的保証そのものである。**
//!
//! 生の応答生成 [`run_and_deliver`] は module-private で、外へ出ているのは
//! per-session ロック込みの [`run_and_deliver_serialized`] だけ。完了受け口
//! （[`crate::sink`]）は**兄弟モジュール**なので、Rust の可視性規則により
//! `run_and_deliver` へ到達できない。したがって「ロックを取らずに応答生成を回す」
//! コードはコンパイルできない。
//!
//! 以前は同一モジュール内の private 子モジュールでこれを表現していたが、その場合
//! 親モジュール（sink と同居）から子の private へは届かないという性質に依存していた。
//! モジュールを分けたことで、子モジュールのトリック無しに同じ保証が得られる。
//! （Nostr の `NostrResponder` は sink と応答生成が同一モジュールにあるため、直呼びを
//!   型で防げていない。そちらの構造は踏襲しない。）

use std::sync::Arc;

use opencrab_actions::{CallerIdentity, RunRequest, SubtaskCompletionSink};
use opencrab_core::runtime_context::prepend_runtime_context;

use crate::gateway::{WebEvent, WEB_SESSION_THEME};
use crate::runner::WebAgentRunner;
use crate::sink::WebCompletionSink;

/// [`run_and_deliver`] を per-session ロックの下で実行する（**唯一の公開入口**）。
///
/// inbound（`POST /api/agents/{id}/web/send`）と resume（[`WebCompletionSink`]）が同じ
/// ロックを通るので、同一セッションに対して 2 本の応答生成が並行しない = 同じ履歴から
/// 2 通の応答が SSE へ流れない。
///
/// ロック取得を呼び出し側の責務にしていた頃は、sink 側の `run_serialized` を外しても
/// テストが全緑だった（呼び忘れを検出できない構造だった）。
pub async fn run_and_deliver_serialized<R: WebAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    caller: CallerIdentity,
    system_prompt_suffix: Option<&str>,
    kind: &str,
) -> Option<String> {
    let fut = run_and_deliver(
        runner,
        agent_id,
        session_id,
        caller,
        system_prompt_suffix,
        kind,
    );
    runner.web_gateway().run_serialized(session_id, fut).await
}

/// 会話を DB から構築 → `run_agent_response`（非ブロック dispatch 付き）→ 応答を
/// DB 保存 ＋ SSE 配送する共通経路。inbound と subtask resume の双方が使う。
///
/// 返り値: 配送した応答本文（NO_REPLY / 空 / エラー時は None）。
///
/// 呼び出しは [`run_and_deliver_serialized`] 経由に限る（直列化の担保のため
/// module-private にしている）。
async fn run_and_deliver<R: WebAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    caller: CallerIdentity,
    system_prompt_suffix: Option<&str>,
    kind: &str,
) -> Option<String> {
    let gateway = runner.web_gateway();

    // 1. system prompt（+ resume 時は [subtask_completed] 注入）。
    let (mut system_prompt, agent_name) = runner.build_agent_context(agent_id);
    if let Some(suffix) = system_prompt_suffix {
        system_prompt = format!("{system_prompt}\n\n{suffix}");
    }

    // 2. 会話文字列（DB から再構築 = 二重回答不変の要）。
    let budget = runner.context_budget_tokens(agent_id);
    let conversation = match runner.build_conversation_string(session_id, agent_id, budget) {
        Ok(raw) => prepend_runtime_context(&raw, WEB_SESSION_THEME),
        Err(e) => {
            tracing::error!(session_id = %session_id, "web run: build_conversation_string failed: {e}");
            return None;
        }
    };

    // 3. 非ブロック dispatch（S3a）を有効化して実行。sink / registry は session 共有。
    let registry = gateway.registry_for(session_id);
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(WebCompletionSink::new(runner.clone()));
    let run_req = RunRequest::new(
        agent_id,
        &agent_name,
        session_id,
        &system_prompt,
        &conversation,
        "web",
        caller,
    )
    .with_dispatch(Some(registry), sink);

    let result = runner.run_agent_response(run_req).await;

    // 4. 応答の保存 ＋ SSE 配送。
    match result {
        Ok(er) if !er.response.trim().is_empty() && er.response.trim() != "NO_REPLY" => {
            runner.record_agent_reply(
                agent_id,
                session_id,
                &er.response,
                er.iterations,
                er.tool_calls_made,
            );
            gateway.publish(
                session_id,
                &WebEvent {
                    kind: kind.to_string(),
                    agent_id: agent_id.to_string(),
                    content: er.response.clone(),
                },
            );
            Some(er.response)
        }
        Ok(_) => None, // NO_REPLY / 空: 配送しない。
        Err(e) => {
            tracing::error!(agent_id = %agent_id, session_id = %session_id, error = format!("{e:#}"), "web run: agent response failed");
            gateway.publish(
                session_id,
                &WebEvent {
                    kind: "error".to_string(),
                    agent_id: agent_id.to_string(),
                    content: format!("(error: {e})"),
                },
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::gateway::web_session_id;
    use crate::testing::FakeRunner;

    /// 応答が SSE へ配送され、転記もされる（inbound の直接応答）。
    #[tokio::test]
    async fn response_is_published_and_recorded() {
        let runner = FakeRunner::new("こんにちは");
        let sid = web_session_id("a", "c1");
        let mut rx = runner.web_gateway().subscribe(&sid);

        let out = run_and_deliver_serialized(
            &runner,
            "a",
            &sid,
            CallerIdentity::TrustedUser,
            None,
            "direct",
        )
        .await;

        assert_eq!(out.as_deref(), Some("こんにちは"));
        let payload = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("SSE へ配送されない")
            .unwrap();
        assert!(payload.contains("\"kind\":\"direct\""));
        assert!(payload.contains("こんにちは"));
        let replies = runner.replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].agent_id, "a");
        assert_eq!(replies[0].session_id, sid);
        assert_eq!(replies[0].text, "こんにちは");
        assert_eq!(runner.runs()[0].session_id, sid);
        // 会話の先頭に実行時コンテキストが付く（#190 S2 の関数を通っている）。
        let conv = runner.runs()[0].conversation.clone();
        assert!(conv.starts_with("[Context]\nCurrent date and time: "));
        assert!(conv.contains(WEB_SESSION_THEME));
        // dispatch（registry + sink）が有効。
        assert!(runner.runs()[0].dispatch_enabled);
    }

    /// NO_REPLY / 空応答は配送も転記もしない（沈黙の尊重）。
    #[tokio::test]
    async fn silence_is_not_delivered() {
        for response in ["NO_REPLY", "   "] {
            let runner = FakeRunner::new(response);
            let sid = web_session_id("a", "silent");
            let mut rx = runner.web_gateway().subscribe(&sid);
            let out = run_and_deliver_serialized(
                &runner,
                "a",
                &sid,
                CallerIdentity::Agent,
                None,
                "direct",
            )
            .await;
            assert!(out.is_none(), "{response:?} は配送しない");
            assert!(runner.replies().is_empty());
            assert!(tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err());
        }
    }

    /// 実行失敗は `kind:"error"` として SSE へ流す（従来挙動）。
    #[tokio::test]
    async fn failure_is_published_as_error_event() {
        let runner = FakeRunner::failing("boom");
        let sid = web_session_id("a", "err");
        let mut rx = runner.web_gateway().subscribe(&sid);

        let out =
            run_and_deliver_serialized(&runner, "a", &sid, CallerIdentity::Agent, None, "direct")
                .await;
        assert!(out.is_none());
        let payload = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("error イベントが流れない")
            .unwrap();
        assert!(payload.contains("\"kind\":\"error\""));
        assert!(payload.contains("boom"));
    }

    /// system prompt の suffix（resume 時の完了マーカー）が渡る。
    #[tokio::test]
    async fn suffix_is_appended_to_system_prompt() {
        let runner = FakeRunner::new("ok");
        let sid = web_session_id("a", "suffix");
        run_and_deliver_serialized(
            &runner,
            "a",
            &sid,
            CallerIdentity::Agent,
            Some("[subtask_completed: subtask_id=st-1, exit_reason=completed]"),
            "subtask_resume",
        )
        .await;
        assert!(runner.runs()[0]
            .system_prompt
            .contains("[subtask_completed: subtask_id=st-1"));
    }

    /// 同一セッションの入口呼び出しは直列化される（入口が必ずロックを取る）。
    #[tokio::test]
    async fn entry_point_serializes_same_session() {
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(80));
        let sid = web_session_id("a", "serial");

        let mut handles = Vec::new();
        for _ in 0..3 {
            let runner = runner.clone();
            let sid = sid.clone();
            handles.push(tokio::spawn(async move {
                run_and_deliver_serialized(
                    &runner,
                    "a",
                    &sid,
                    CallerIdentity::Agent,
                    None,
                    "direct",
                )
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            runner.max_inflight(),
            1,
            "同一セッションの応答生成は同時に 1 本まで（二重回答の防止）"
        );
        assert_eq!(runner.runs().len(), 3);
    }
}
