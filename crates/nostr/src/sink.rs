//! Nostr の応答生成経路と subtask 完了 sink（#168 / RFC #152 S3b-1）。
//!
//! Nostr は「inbound イベントへの応答」と「background subtask 完了後の resume」の
//! 2 経路で同じことをする: 会話を DB から再構築 → `run_agent_response` → 返信。
//! その共通経路を [`NostrResponder`] に置き、[`SubtaskCompletionSink`] 実装も同じ型が
//! 担う（web gateway の `WebCompletionSink` + `run_and_deliver` と同じ構造）。
//! ただし web は応答生成と sink を別モジュールに分けており、sink から生の応答生成へ
//! 到達できない（直列化の飛ばしがコンパイルエラーになる）。ここは同一モジュールなので
//! その保証が無く、`respond_serialized` 経由という規律に頼っている。
//!
//! 不変条件（RFC §6）:
//! - **二重回答しない**: `settle_completed` が「DB 永続化 → sink 発火」の順序を保証済み。
//!   resume は `build_conversation_string` で DB から会話を再構築するため、完了本文を
//!   sink で運ぶ必要がない。
//! - **per-session 直列化**: inbound と resume の応答生成をどちらも
//!   [`NostrSessionRuntime::run_serialized`] の下で走らせる。同一セッション（相手）に
//!   対して 2 本の応答生成が並行しないので、二重投稿にならない。
//! - **二重投稿しない（Nostr 固有）**: モデルが `nostr_*` で明示送信していれば
//!   `sent_flag` が立ち、ループ側の暗黙返信を抑制する。この判定が成り立つのは
//!   配送系ツールが **inline 実行**（`NOSTR_DELIVERY_ACTIONS` = dispatch 除外）で
//!   あることが前提。background 化すると run 終了時にまだ送信されておらず、
//!   暗黙返信と後追いの明示送信で二重投稿になる。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tracing::{debug, error, warn};

use opencrab_actions::{
    CallerIdentity, RunRequest, SettleKind, SubtaskCompletionSink, SubtaskSettled,
};
use opencrab_gateway::GatewayActions;

use crate::actions::NostrGatewayActions;
use crate::cli::NostaroCli;
use crate::identity::NostrIdentityAdmin;
use crate::runner::NostrAgentRunner;
use crate::session::{NostrSessionRuntime, NOSTR_SESSION_PREFIX};

/// Nostr の応答生成 + 返信配送の実体。`SubtaskCompletionSink` も実装する。
///
/// watch ループ（inbound）と完了 sink（resume）が同じ `runtime`（session ロック +
/// registry）・同じ `cli`（送信）・同じ `admin`（identity 切替）を共有する。
pub struct NostrResponder<R: NostrAgentRunner> {
    runner: R,
    cli: NostaroCli,
    runtime: Arc<NostrSessionRuntime>,
    admin: Arc<dyn NostrIdentityAdmin>,
    agent_id: String,
}

impl<R: NostrAgentRunner> Clone for NostrResponder<R> {
    fn clone(&self) -> Self {
        Self {
            runner: self.runner.clone(),
            cli: self.cli.clone(),
            runtime: self.runtime.clone(),
            admin: self.admin.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

impl<R: NostrAgentRunner> NostrResponder<R> {
    pub fn new(
        runner: R,
        cli: NostaroCli,
        runtime: Arc<NostrSessionRuntime>,
        admin: Arc<dyn NostrIdentityAdmin>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            runner,
            cli,
            runtime,
            admin,
            agent_id: agent_id.into(),
        }
    }

    pub fn runtime(&self) -> &Arc<NostrSessionRuntime> {
        &self.runtime
    }

    /// [`Self::respond`] を per-session ロックの下で実行する（唯一の公開入口）。
    ///
    /// inbound（watch ループ）と resume（完了 sink）が同じロックを通るので、同一
    /// セッションに対して 2 本の応答生成が並行しない = 二重投稿しない。ロック取得を
    /// 呼び出し側の責務にすると 1 箇所の忘れで不変条件が壊れるため、ここに閉じ込める。
    pub async fn respond_serialized(
        &self,
        session_id: &str,
        reply_target: &str,
        prompt_suffix: &str,
        trigger_message_id: Option<&str>,
    ) -> Option<String> {
        let fut = self.respond(session_id, reply_target, prompt_suffix, trigger_message_id);
        self.runtime.run_serialized(session_id, fut).await
    }

    /// 会話を DB から再構築 → `run_agent_response`（非ブロック dispatch 付き）→
    /// 応答を転記し、明示送信が無ければ `reply_target` へ暗黙返信する共通経路。
    ///
    /// 呼び出しは [`Self::respond_serialized`] 経由に限る（直列化の担保）。
    /// 返り値は配送した応答本文（沈黙時は `None`）。
    async fn respond(
        &self,
        session_id: &str,
        reply_target: &str,
        prompt_suffix: &str,
        trigger_message_id: Option<&str>,
    ) -> Option<String> {
        let agent_id = self.agent_id.as_str();
        let (base_prompt, agent_name) = self.runner.build_agent_context(agent_id);
        let system_prompt = format!("{base_prompt}\n\n{prompt_suffix}");

        let budget = self.runner.context_budget_tokens(agent_id);
        let conversation = self
            .runner
            .build_conversation_string(session_id, agent_id, budget)
            .unwrap_or_default();

        // 明示送信フラグ（暗黙返信の二重投稿防止）。配送系ツールは dispatch 除外
        // （`NOSTR_DELIVERY_ACTIONS`）なので、run が返る時点で送信は済んでいる。
        let gw = NostrGatewayActions::new(self.cli.clone()).with_admin(self.admin.clone());
        let sent = gw.sent_flag();
        let actions: Arc<dyn GatewayActions> = Arc::new(gw);

        // dispatch（S3a）: registry は session 単位で共有し（cancel_subtask 到達性）、
        // sink は自分自身（完了したらまた直列化下で resume する）。
        let registry = self.runtime.registry_for(session_id);
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(self.clone());

        let mut req = RunRequest::new(
            agent_id,
            agent_name,
            session_id,
            system_prompt,
            conversation,
            "nostr",
            // Nostr の投稿者は外部ユーザー。最小権限（Agent）で扱う。
            CallerIdentity::Agent,
        )
        .with_gateway_actions(actions)
        .with_dispatch(Some(registry), sink)
        .with_reply_target(reply_target);
        if let Some(id) = trigger_message_id {
            req = req.with_trigger_message_id(id);
        }

        match self.runner.run_agent_response(req).await {
            Ok(result) => {
                let reply = result.response.trim().to_string();
                if reply.is_empty() || reply == "NO_REPLY" {
                    debug!(agent_id, session_id, "nostr: agent chose silence");
                    return None;
                }
                // 最終応答テキストを転記（会話履歴の継続性）。
                self.runner.record_outbound_reply(
                    opencrab_actions::TranscriptSource::Nostr,
                    &opencrab_actions::OutboundReplyRecord {
                        agent_id,
                        session_id,
                        channel_id: None,
                        text: &reply,
                        context: None,
                    },
                );
                // モデルが既に nostr_* で送信していれば暗黙返信しない（二重送信防止）。
                if sent.load(Ordering::SeqCst) {
                    debug!(
                        agent_id,
                        session_id,
                        "nostr: explicit send already occurred; skipping implicit reply"
                    );
                    return Some(reply);
                }
                if let Err(e) = self.cli.reply(agent_id, reply_target, &reply, None).await {
                    warn!(agent_id, error = %e, "nostr implicit reply failed");
                }
                Some(reply)
            }
            Err(e) => {
                error!(agent_id, session_id, error = %e, "nostr agent run failed");
                None
            }
        }
    }
}

/// resume 時に system prompt へ足す Nostr 固有の指示を組む。
fn resume_prompt_suffix(reply_target: &str, subtask_id: &str, exit_reason: &str) -> String {
    format!(
        "[Nostr] 依頼されていたバックグラウンド処理が完了しました。結果は直前の会話ログの \
         subtask_completed に入っています。相手へ伝えるなら nostr_reply(target=\"{reply_target}\") \
         を使ってください（target は返信先ノート）。伝える必要がなければ NO_REPLY とだけ答えてください。\
         \n[subtask_completed: subtask_id={subtask_id}, exit_reason={exit_reason}]"
    )
}

impl<R: NostrAgentRunner> SubtaskCompletionSink for NostrResponder<R> {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        // 決着（Completed）以外（進捗通知など）で resume すると、まだ走っている run の
        // 途中で二重に応答してしまう。型の意図をここで実際に守る。
        if ev.kind != SettleKind::Completed {
            debug!(
                session_id = %ev.session_id,
                kind = ?ev.kind,
                "nostr sink: not a completion, skipping resume"
            );
            return;
        }
        // 非 Nostr の親セッション（heartbeat-* / web-* / ネストした subtask-* 等）は
        // 正常系としてスキップする（Discord / web の sink も同様に前置きで弾く）。
        if !ev.session_id.starts_with(NOSTR_SESSION_PREFIX) {
            debug!(
                session_id = %ev.session_id,
                "nostr sink: parent session is not a nostr session, skipping resume"
            );
            return;
        }
        // 返信先が無ければ **resume しない**（方針 / #168）。
        //
        // Nostr は「返信して初めて相手に届く」gateway で、session_id からは返信先ノート
        // を復元できない（相手 pubkey しか入っていない）。宛先不明のまま resume すると
        // (1) 届かない応答を生成して LLM 費用を払い、(2) その本文を会話ログに転記して
        // しまう（送っていないのに送ったことになり、以後の文脈が実際の Nostr 上のやり取り
        // と食い違う）。完了本文は `settle_completed` が既に DB へ永続化しているので、
        // 次の inbound で `build_conversation_string` が自然に拾う（heartbeat の
        // 「次 tick 拾い」と同じ扱い）。取りこぼしではなく遅延配送になる。
        let reply_target = ev.reply_target.clone().unwrap_or_default();
        if reply_target.trim().is_empty() {
            debug!(
                session_id = %ev.session_id,
                subtask_id = %ev.subtask_id,
                "nostr sink: no reply_target; skipping resume (完了本文は DB 済み。次の inbound で文脈に載る)"
            );
            return;
        }

        let responder = self.clone();
        let sid = ev.session_id.clone();
        // sink は同期関数。resume は非同期なので spawn する（web gateway と同じ。
        // ここで待つと dispatch した subtask の完了処理を塞ぐ）。
        tokio::spawn(async move {
            let suffix = resume_prompt_suffix(&reply_target, &ev.subtask_id, &ev.exit_reason);
            // inbound の応答生成と直列化する（同一セッションで二重に返信しない）。
            responder
                .respond_serialized(&sid, &reply_target, &suffix, None)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;
    use std::time::Duration;

    use opencrab_actions::SubtaskSettled;
    use opencrab_core::EngineResult;
    use opencrab_db::queries::AgentNostrConfigRow;

    use crate::session::nostr_session_id;

    /// run_agent_response の観測 1 件
    /// （session_id, reply_target, dispatch 有効か, run に載った登録簿の実体）。
    ///
    /// 4 番目は **`Arc` の同一性**を見るために保持する。「dispatch が有効か」（3 番目の
    /// bool）だけでは、別インスタンスの登録簿を渡す壊れ方を検知できない。
    type RunObservation = (
        String,
        Option<String>,
        bool,
        Option<opencrab_actions::subtask::SubtaskRegistry>,
    );
    /// 転記された応答 1 件（agent_id, session_id, text）。
    type ReplyObservation = (String, String, String);

    /// テスト用の最小 `NostrAgentRunner`。LLM も DB も使わず、応答を差し替える。
    #[derive(Clone)]
    struct FakeRunner {
        response: String,
        runs: Arc<Mutex<Vec<RunObservation>>>,
        replies: Arc<Mutex<Vec<ReplyObservation>>>,
        /// run 中の待機（直列化テスト用）。
        delay: Duration,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        /// Some のとき「モデルが inline で nostr_reply を呼んだ」ことを模して、
        /// 渡された gateway_actions を実際に実行する（sent フラグ経路の検証）。
        explicit_reply_target: Option<String>,
    }

    impl FakeRunner {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
                runs: Arc::new(Mutex::new(Vec::new())),
                replies: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::from_millis(0),
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                explicit_reply_target: None,
            }
        }

        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }

        /// 「モデルがターン中に nostr_reply を明示実行する」挙動を仕込む。
        fn with_explicit_reply(mut self, target: &str) -> Self {
            self.explicit_reply_target = Some(target.to_string());
            self
        }
    }

    #[async_trait::async_trait]
    impl opencrab_actions::AgentRuntime for FakeRunner {
        async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            self.runs.lock().unwrap().push((
                req.session_id.clone(),
                req.reply_target.clone(),
                req.completion_sink.is_some() && req.subtask_registry.is_some(),
                req.subtask_registry.clone(),
            ));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            // モデルが配送系ツールを inline 実行するケース（sent フラグを立てる経路）。
            if let (Some(target), Some(ga)) =
                (&self.explicit_reply_target, req.gateway_actions.as_ref())
            {
                let ctx = opencrab_gateway::GatewayCallContext::for_agent(&req.agent_id);
                let r = ga
                    .execute(
                        "nostr_reply",
                        &serde_json::json!({"target": target, "text": "明示送信"}),
                        &ctx,
                    )
                    .await;
                assert!(
                    r.success,
                    "fake nostaro での明示送信は成功する: {:?}",
                    r.error
                );
            }
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(EngineResult {
                response: self.response.clone(),
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                xml_fallback_parses: 0,
            })
        }

        fn build_agent_context(&self, _agent_id: &str) -> (String, String) {
            ("base prompt".to_string(), "テストくん".to_string())
        }

        fn build_conversation_string(
            &self,
            _session_id: &str,
            _agent_id: &str,
            _budget: usize,
        ) -> anyhow::Result<String> {
            Ok("conversation".to_string())
        }

        fn context_budget_tokens(&self, _agent_id: &str) -> usize {
            1000
        }

        fn has_llm_providers(&self) -> bool {
            true
        }

        fn ensure_session(&self, _s: &str, _a: &[String], _t: &str, _m: &str, _mode: &str) {}

        fn record_outbound_reply(
            &self,
            source: opencrab_actions::TranscriptSource,
            record: &opencrab_actions::OutboundReplyRecord<'_>,
        ) {
            assert_eq!(source, opencrab_actions::TranscriptSource::Nostr);
            self.replies.lock().unwrap().push((
                record.agent_id.to_string(),
                record.session_id.to_string(),
                record.text.to_string(),
            ));
        }

        // 以下はこの sink の経路が使わない（受信転記/NO_REPLY/掃除）。
        fn record_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) -> bool {
            unimplemented!("nostr の fake は受信転記を使わない")
        }

        fn on_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _agent_id: &str,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) {
            unimplemented!("nostr の fake は受信フックを使わない")
        }

        fn record_interaction_response(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _record: &opencrab_actions::InteractionRecord<'_>,
        ) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {
            unimplemented!("nostr の fake は NO_REPLY 記録を使わない")
        }

        fn session_theme(&self, _session_id: &str) -> Option<String> {
            unimplemented!("nostr の fake は session_theme を使わない")
        }

        fn mark_interaction_status(&self, _i: &str, _s: &str, _r: Option<&str>, _u: Option<&str>) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions(&self) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }

        fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {
            unimplemented!("nostr の fake は A2UI interaction を使わない")
        }
    }

    impl NostrAgentRunner for FakeRunner {
        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            None
        }

        fn set_nostr_secret_key(&self, _agent_id: &str, _secret_key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn upsert_nostr_config(&self, _cfg: &AgentNostrConfigRow) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_nostr_enabled(&self, _agent_id: &str, _enabled: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn resolve_nostr_relay_target(
            &self,
            _agent_id: &str,
        ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
            // この経路（応答生成 sink）は転記に関与しないので未設定扱い。
            None
        }

        fn relay_inbound_notification(
            &self,
            _target: &opencrab_actions::webhook_target::WebhookConfig,
            _text: String,
        ) {
        }
    }

    struct NoopAdmin;

    #[async_trait::async_trait]
    impl NostrIdentityAdmin for NoopAdmin {
        async fn adopt_generated_identity(
            &self,
            _agent_id: &str,
            npub: &str,
        ) -> anyhow::Result<String> {
            Ok(npub.to_string())
        }
    }

    /// 送信を観測するための fake nostaro（argv を 1 行ずつ log へ追記するスクリプト）。
    /// 実リレーへは一切繋がない。
    struct FakeNostaro {
        _dir: tempfile::TempDir,
        script: std::path::PathBuf,
        log: std::path::PathBuf,
    }

    impl FakeNostaro {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("sent.log");
            let script = dir.path().join("fake-nostaro.sh");
            std::fs::write(
                &script,
                format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self {
                _dir: dir,
                script,
                log,
            }
        }

        fn cli(&self) -> NostaroCli {
            NostaroCli::new().with_binary_path(self.script.to_string_lossy().to_string())
        }

        fn sent(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }

        /// log に `needle` が現れるまで待つ（最大 2 秒）。
        async fn wait_for(&self, needle: &str) -> bool {
            for _ in 0..200 {
                if self.sent().contains(needle) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }
    }

    fn responder(runner: FakeRunner, cli: NostaroCli) -> NostrResponder<FakeRunner> {
        NostrResponder::new(
            runner,
            cli,
            Arc::new(NostrSessionRuntime::new()),
            Arc::new(NoopAdmin),
            "agent-sink-test",
        )
    }

    fn settled(session_id: &str, reply_target: Option<&str>) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-sink-test".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind: SettleKind::Completed,
            reply_target: reply_target.map(|s| s.to_string()),
        }
    }

    /// sink は `reply_target` 宛に返信する（session_id からは復元できない宛先）。
    #[tokio::test]
    async fn sink_replies_to_reply_target() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("鍵ができました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-abc");

        r.on_subtask_settled(settled(&sid, Some("note1target")));

        assert!(
            fake.wait_for("note1target").await,
            "reply_target 宛に返信されるべき: log={}",
            fake.sent()
        );
        let sent = fake.sent();
        assert!(sent.contains("reply"), "reply サブコマンドで送る: {sent}");
        assert!(sent.contains("鍵ができました"));
        // resume も dispatch 有効（registry + sink）で走り、reply_target を引き継ぐ。
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, sid);
        assert_eq!(runs[0].1.as_deref(), Some("note1target"));
        assert!(runs[0].2, "resume も非ブロック dispatch を有効化する");
    }

    /// `reply_target` が None のときは graceful にスキップ（resume も送信もしない）。
    #[tokio::test]
    async fn sink_without_reply_target_is_graceful() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("届かない応答");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-abc");

        r.on_subtask_settled(settled(&sid, None));
        // 空文字も「指定なし」扱い。
        r.on_subtask_settled(settled(&sid, Some("   ")));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            fake.sent().is_empty(),
            "宛先不明なら送信しない: {}",
            fake.sent()
        );
        assert!(
            runner.runs.lock().unwrap().is_empty(),
            "宛先不明なら LLM も回さない（費用と未配送転記の防止）"
        );
    }

    /// 非 Nostr セッションの settle は無視する（web / heartbeat のネスト等）。
    #[tokio::test]
    async fn sink_ignores_non_nostr_sessions() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("x");
        let r = responder(runner.clone(), fake.cli());

        r.on_subtask_settled(settled("web-agent-x-conv1", Some("note1target")));
        r.on_subtask_settled(settled("heartbeat-agent-x", Some("note1target")));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(fake.sent().is_empty());
        assert!(runner.runs.lock().unwrap().is_empty());
    }

    /// NO_REPLY / 空応答なら送信しない（沈黙の尊重）。
    #[tokio::test]
    async fn no_reply_response_is_not_delivered() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("NO_REPLY");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-abc");

        let out = r
            .respond_serialized(&sid, "note1target", "suffix", None)
            .await;
        assert!(out.is_none());
        assert!(fake.sent().is_empty());
        // 転記もしない（送っていない応答を履歴に残さない）。
        assert!(runner.replies.lock().unwrap().is_empty());
    }

    /// 二重投稿しない（#168 の核）: モデルがターン中に `nostr_reply` を明示実行したら
    /// `sent` フラグが立ち、暗黙返信は送らない。送信は 1 回だけ。
    ///
    /// この不変条件は配送系ツールが **inline 実行**（dispatch 除外）であることに依存する。
    /// background 化されると run が返る時点でフラグが立っておらず、暗黙返信＋後追いの
    /// 明示送信で 2 通になる。除外集合の側は `test_nostr_delivery_actions_are_non_dispatch`
    /// （`crates/nostr/src/actions.rs`）が守る。
    #[tokio::test]
    async fn explicit_send_suppresses_implicit_reply() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("本文").with_explicit_reply("note1explicit");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-dup");

        let out = r
            .respond_serialized(&sid, "note1implicit", "suffix", Some("evt-1"))
            .await;
        assert_eq!(out.as_deref(), Some("本文"));

        // 明示送信の 1 通だけ。暗黙返信（note1implicit 宛）は送らない。
        let sent = fake.sent();
        assert!(sent.contains("note1explicit"), "明示送信が届く: {sent}");
        assert!(
            !sent.contains("note1implicit"),
            "明示送信済みなら暗黙返信しない（二重投稿の防止）: {sent}"
        );
        assert_eq!(
            sent.lines().filter(|l| l.contains("reply")).count(),
            1,
            "送信は 1 回だけ: {sent}"
        );
        // 応答本文の転記は行う（会話履歴の継続性）。
        assert_eq!(runner.replies.lock().unwrap().len(), 1);
    }

    /// 明示送信が無ければ `reply_target` 宛に暗黙返信する（従来挙動の保持）。
    #[tokio::test]
    async fn implicit_reply_is_sent_when_no_explicit_send() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("暗黙で返す");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-implicit");

        r.respond_serialized(&sid, "note1implicit", "suffix", Some("evt-1"))
            .await;
        let sent = fake.sent();
        assert!(sent.contains("note1implicit"), "{sent}");
        assert!(sent.contains("暗黙で返す"), "{sent}");
        assert_eq!(sent.lines().filter(|l| l.contains("reply")).count(), 1);
    }

    /// 同一セッションでは inbound 相当の respond と resume が直列化される。
    #[tokio::test]
    async fn resume_serializes_with_inbound_on_same_session() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(120));
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-serial");

        // inbound 相当（watch ループと同じ入口）を走らせつつ、途中で完了 sink を発火。
        let r2 = r.clone();
        let sid2 = sid.clone();
        let inbound = tokio::spawn(async move {
            r2.respond_serialized(&sid2, "note1inbound", "suffix", Some("evt-1"))
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        r.on_subtask_settled(settled(&sid, Some("note1resume")));

        inbound.await.unwrap();
        assert!(fake.wait_for("note1resume").await, "resume も配送される");
        // 直列化されているので LLM 実行が重なることはない。
        assert_eq!(
            runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "同一セッションの応答生成は同時に 1 本まで（二重回答の防止）"
        );
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
    }

    /// 別セッション（別の相手）は直列化されず並行する。
    #[tokio::test]
    async fn different_sessions_are_not_serialized() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(150));
        let r = responder(runner.clone(), fake.cli());

        r.on_subtask_settled(settled(
            &nostr_session_id("agent-sink-test", "pk-a"),
            Some("note1a"),
        ));
        r.on_subtask_settled(settled(
            &nostr_session_id("agent-sink-test", "pk-b"),
            Some("note1b"),
        ));

        assert!(fake.wait_for("note1a").await);
        assert!(fake.wait_for("note1b").await);
        assert!(
            runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
            "別セッションは並行して走れる"
        );
    }

    /// dispatch した subtask は session 共有 registry に載り、`cancel_subtask` から
    /// 到達できる（別 registry を渡すと常に not found になる回帰の防止 / #169）。
    #[tokio::test]
    async fn registry_is_shared_between_inbound_and_resume() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test", "pk-reg");

        let inbound_registry = r.runtime().registry_for(&sid);

        // **応答生成に実際に渡された登録簿**が、停止処理が引くものと同一 Arc であること。
        //
        // ここを `registry_for(&sid)` 同士の比較で書くと `SubtaskRegistries` の恒真式に
        // なり、`respond` 側が別インスタンスを渡す壊れ方を 1 件も検知できない（実際、
        // 旧テストは `sink.rs` の `registry_for(session_id)` を新規 DashMap に差し替えても
        // 緑のままだった / #203 の一括点検）。捕まえたいのは配線なので、`FakeRunner` が
        // 捕捉した `RunRequest` の中身を見る（`web-gateway` の
        // `run_uses_the_gateways_registry_so_cancel_can_reach_it` と同じ形）。
        //
        // inbound（watch ループの入口）と resume（完了 sink）の**両経路**を見る:
        // どちらか一方だけ配線が外れても停止が届かなくなる。
        r.respond_serialized(&sid, "note1inbound", "suffix", Some("evt-1"))
            .await;
        r.on_subtask_settled(settled(&sid, Some("note1resume")));
        assert!(fake.wait_for("note1resume").await, "resume が走ること");

        {
            let runs = runner.runs.lock().unwrap();
            assert_eq!(runs.len(), 2, "inbound と resume で 2 回走る");
            for (label, obs) in [("inbound", &runs[0]), ("resume", &runs[1])] {
                let observed = obs
                    .3
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}: run に登録簿が載っていない"));
                assert!(
                    Arc::ptr_eq(observed, &inbound_registry),
                    "{label}: 応答生成に渡した登録簿が、停止処理が引くものと別インスタンス\
                     になっている（cancel_subtask が常に not found になる）"
                );
            }
        }

        // 走行中 subtask を模して登録 → has_running が真。
        inbound_registry.insert(
            "st-live".to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: "subtask-st-live".to_string(),
                parent_session_id: sid.clone(),
                agent_id: "agent-sink-test".to_string(),
                label: "nostr_generate_key(sunny)".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: Some("note1target".to_string()),
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        assert!(r.runtime().has_running(&sid));

        // 同じ registry を引く `cancel_subtask`（server-neutral / #161）で停止できる。
        let db = opencrab_db::Db::memory().unwrap();
        let outcome = opencrab_actions::cancel_subtask(
            &r.runtime().registry_for(&sid),
            &db,
            None,
            None,
            "st-live",
            false,
            Some(&sid),
        );
        assert_eq!(outcome, opencrab_actions::CancelOutcome::Cancelled);
        assert!(!r.runtime().has_running(&sid));
    }
}
