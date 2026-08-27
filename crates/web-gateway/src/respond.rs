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

/// web の公開ターン入口の結果（#632）。
///
/// - `Ran`: ターンを走らせて配送した応答本文（NO_REPLY / 空 / 実行エラーのときは `None`）。
/// - `AgentNotFound`: `agents` 行が無く**ターンを起こさなかった**。HTTP ハンドラは 404 に写像する。
/// - `Error`: 存在確認そのものが DB エラーで失敗した。**404 ではない**（一過性の障害で
///   実在するエージェントを 404 に化けさせない）。HTTP ハンドラは 500 に写像する。
///
/// `#[must_use]` は付けない: resume（[`crate::sink`]）と timed-fire（[`crate::fire`]）は
/// 返り値を使わず discard するため（存在しないエージェントなら単に何もしないのが正しい）。
#[derive(Debug)]
pub enum WebTurnOutcome {
    /// ターンを走らせて応答を配送した（`None` は NO_REPLY / 空 / 実行エラー）。
    Ran(Option<String>),
    /// `agents` 行が無くターンを起こさなかった（#632）。
    AgentNotFound,
    /// 存在確認が DB エラーで失敗した（#632）。404 ではなく内部エラー。
    Error(String),
}

/// [`run_and_deliver`] を per-session ロックの下で実行する（**唯一の公開入口**）。
///
/// inbound（`POST /api/agents/{id}/web/send`）と resume（[`WebCompletionSink`]）が同じ
/// ロックを通るので、同一セッションに対して 2 本の応答生成が並行しない = 同じ履歴から
/// 2 通の応答が SSE へ流れない。
///
/// ロック取得を呼び出し側の責務にしていた頃は、sink 側の `run_serialized` を外しても
/// テストが全緑だった（呼び忘れを検出できない構造だった）。
///
/// **#632: web の全ターンがこの 1 本に閉じている（#177）。ここで存在確認を 1 度だけ行い、
/// 行が無ければ [`WebTurnOutcome::AgentNotFound`] を返して以降（会話構築・LLM 実行・
/// 応答配送）へ進まない。** production では下流の `run_agent_response` にもサーバ側の
/// チョークポイントがあるが、ここで先に弾くことで DB へ応答を残さず・SSE へ流さず・
/// resume/fire にも同じ判定を効かせられる（web-gateway クレート単体で完結する）。
///
/// 存在確認が DB エラーで失敗したときは [`WebTurnOutcome::Error`] を返す（**404 ではない**。
/// 一過性の障害で実在するエージェントを 404 に化けさせないため）。サーバ側の
/// `get_agent(...)?` が Err を伝播させるのと同じ方針。
pub async fn run_and_deliver_serialized<R: WebAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    caller: CallerIdentity,
    system_prompt_suffix: Option<&str>,
    kind: &str,
) -> WebTurnOutcome {
    match runner.agent_exists(agent_id) {
        Ok(true) => {}
        Ok(false) => return WebTurnOutcome::AgentNotFound,
        Err(e) => {
            tracing::error!(agent_id = %agent_id, error = format!("{e:#}"), "web run: 存在確認が DB エラーで失敗");
            return WebTurnOutcome::Error(format!("failed to check agent existence: {e}"));
        }
    }
    let fut = run_and_deliver(
        runner,
        agent_id,
        session_id,
        caller,
        system_prompt_suffix,
        kind,
    );
    WebTurnOutcome::Ran(runner.web_gateway().run_serialized(session_id, fut).await)
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
    //    #352: 本ターンの caller で index を絞る（同じ caller を下の RunRequest にも載せる）。
    let (mut system_prompt, agent_name) = runner.build_agent_context(agent_id, &caller);
    if let Some(suffix) = system_prompt_suffix {
        system_prompt = format!("{system_prompt}\n\n{suffix}");
    }

    // 2. 会話文字列（DB から再構築 = 二重回答不変の要）。
    let runtime_text = prepend_runtime_context("", WEB_SESSION_THEME);
    let budget =
        match runner.context_budget_tokens(agent_id, session_id, &system_prompt, &runtime_text) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error_name = e.name(),
                    "web run: {name}: {e}",
                    name = e.name()
                );
                return None;
            }
        };
    let conversation = match runner.build_conversation_string(
        session_id,
        agent_id,
        budget,
        &system_prompt,
        &runtime_text,
    ) {
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

        assert!(
            matches!(&out, WebTurnOutcome::Ran(Some(t)) if t == "こんにちは"),
            "unexpected outcome: {out:?}"
        );
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
            assert!(
                matches!(out, WebTurnOutcome::Ran(None)),
                "{response:?} は配送しない"
            );
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
        assert!(matches!(out, WebTurnOutcome::Ran(None)));
        let payload = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("error イベントが流れない")
            .unwrap();
        assert!(payload.contains("\"kind\":\"error\""));
        assert!(payload.contains("boom"));
    }

    /// **存在しないエージェントはこの入口でターンを起こさない**（#632）。
    ///
    /// web の全ターンがこの 1 本を通るので、ここで弾けば resume / timed-fire を含めて
    /// 閉じる。`AgentNotFound` を返し、`run_agent_response` へ進まない（配送も転記もしない）。
    #[tokio::test]
    async fn unknown_agent_is_not_run() {
        let runner = FakeRunner::new("走ってはいけない").without_agent();
        let sid = web_session_id("ghost", "c1");
        let out = run_and_deliver_serialized(
            &runner,
            "ghost",
            &sid,
            CallerIdentity::TrustedUser,
            None,
            "direct",
        )
        .await;
        assert!(
            matches!(out, WebTurnOutcome::AgentNotFound),
            "存在しないエージェントは AgentNotFound を返す: {out:?}"
        );
        assert!(runner.runs().is_empty(), "ターンが走ってはいけない");
        assert!(runner.replies().is_empty(), "応答を転記してはいけない");
    }

    /// **存在確認の DB エラーは 404 ではなく `Error`**（#632 レビュー指摘）。
    ///
    /// 一過性の DB エラーで実在するエージェントを 404 に化けさせない。`AgentNotFound` に
    /// 潰さず `Error` を返し、ターンも走らせない。
    #[tokio::test]
    async fn db_error_during_existence_check_is_not_agent_not_found() {
        let runner = FakeRunner::new("走ってはいけない").failing_agent_exists("db is down");
        let sid = web_session_id("real", "c1");
        let out = run_and_deliver_serialized(
            &runner,
            "real",
            &sid,
            CallerIdentity::TrustedUser,
            None,
            "direct",
        )
        .await;
        assert!(
            matches!(&out, WebTurnOutcome::Error(e) if e.contains("db is down")),
            "DB エラーは AgentNotFound ではなく Error: {out:?}"
        );
        assert!(
            runner.runs().is_empty(),
            "DB エラー時にターンが走ってはいけない"
        );
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

    /// envelope に渡す system / runtime は、同じターンの実 request と一致する。
    #[tokio::test]
    async fn envelope_uses_actual_system_and_runtime() {
        let runner = FakeRunner::new("ok");
        let sid = web_session_id("a", "envelope");
        run_and_deliver_serialized(
            &runner,
            "a",
            &sid,
            CallerIdentity::TrustedUser,
            Some("[subtask_completed: subtask_id=st-1, exit_reason=completed]"),
            "subtask_resume",
        )
        .await;
        let calls = runner.envelope_calls();
        assert!(
            !calls.is_empty(),
            "context_budget_tokens / build_conversation_string が実 prompt を受け取る"
        );
        let run = &runner.runs()[0];
        for (system, runtime) in &calls {
            assert_eq!(system, &run.system_prompt);
            assert!(runtime.starts_with("[Context]\n"), "{runtime}");
            assert!(runtime.contains(WEB_SESSION_THEME), "{runtime}");
        }
        assert!(run.conversation.starts_with("[Context]\n"));
        assert!(run.conversation.contains(WEB_SESSION_THEME));
    }

    /// **run に載る登録簿は、停止処理が引くものと同一の Arc でなければならない**。
    ///
    /// 停止（cancel）は「セッションの登録簿から subtask を引く」実装なので、応答生成に
    /// 別の登録簿を渡すと走行中の subtask が見つからず、停止が常に失敗する。ここを壊しても
    /// 落ちるテストが無い状態だったので固定する（同一性の検査なので `Arc::ptr_eq` で見る）。
    #[tokio::test]
    async fn run_uses_the_gateways_registry_so_cancel_can_reach_it() {
        let runner = FakeRunner::new("ok");
        let sid = web_session_id("a", "c-registry");

        run_and_deliver_serialized(
            &runner,
            "a",
            &sid,
            CallerIdentity::TrustedUser,
            None,
            "direct",
        )
        .await;

        let runs = runner.runs();
        let observed = runs
            .first()
            .and_then(|r| r.subtask_registry.clone())
            .expect("run に登録簿が載っていない（非ブロック実行が無効）");
        let from_gateway = runner.web_gateway().registry_for(&sid);
        assert!(
            std::sync::Arc::ptr_eq(&observed, &from_gateway),
            "応答生成に渡した登録簿が、停止処理が引くものと別インスタンスになっている"
        );
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
