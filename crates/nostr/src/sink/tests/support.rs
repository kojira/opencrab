    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;
    use std::time::Duration;

    use opencrab_actions::{SettleKind, SubtaskSettled};
    use opencrab_core::EngineResult;
    use opencrab_db::queries::AgentNostrConfigRow;

    use crate::session::nostr_session_id;

    /// run_agent_response の観測 1 件
    /// （session_id, reply_target, dispatch 有効か, run に載った登録簿の実体）。
    ///
    /// 4 番目は **`Arc` の同一性**を見るために保持する。「dispatch が有効か」（3 番目の
    /// bool）だけでは、別インスタンスの登録簿を渡す壊れ方を検知できない。
    /// 5 番目は **run に載った呼び出し元**（#319）。以前はここが常に `Agent` 固定で、
    /// オーナー発のターンでも OWNER_ONLY / TRUSTED_ONLY のツールが出なかった。
    type RunObservation = (
        String,
        Option<String>,
        bool,
        Option<opencrab_actions::subtask::SubtaskRegistry>,
        CallerIdentity,
        // 6 番目: 走行中注入の対象範囲（#323 / B2）。sink が respond の scope を
        // RunRequest へ配線していることを検査する（ラベル文字列で保持）。
        String,
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
        /// #588 Stage 2: 1 つだけ保持し `session_locks()` は毎回この clone を返す
        /// （trait の「プロセス全体で 1 実体を共有」契約を fake でも守る）。
        session_locks: std::sync::Arc<opencrab_actions::SessionLocks>,
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
                session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
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

        /// #588: 配送は機構が行わなくなったので、resume / inbound が「走ってセッションへ
        /// 転記された」ことは**記録**（`replies`）で観測する（旧テストが送信ログ `fake.sent()` を
        /// 同期点に使っていた箇所の置き換え）。転記本文のどれかが `needle` を含めば true。
        async fn wait_for_reply(&self, needle: &str) -> bool {
            for _ in 0..100 {
                if self
                    .replies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.2.contains(needle))
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }
    }

    #[async_trait::async_trait]
    impl opencrab_actions::AgentRuntime for FakeRunner {
        async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            let scope_label = match &req.live_inbound_scope {
                opencrab_actions::LiveInboundScope::AllOthers => "all".to_string(),
                opencrab_actions::LiveInboundScope::OnlySpeaker(pk) => format!("only:{pk}"),
                opencrab_actions::LiveInboundScope::Silent => "silent".to_string(),
            };
            self.runs.lock().unwrap().push((
                req.session_id.clone(),
                req.reply_target.clone(),
                req.completion_sink.is_some() && req.subtask_registry.is_some(),
                req.subtask_registry.clone(),
                req.caller.clone(),
                scope_label,
            ));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            // 組み込み nostr_reply は撤去済み（fail-closed）。sink 経由の
            // publish 副経路は残さない（返信は V3/DI reply 操作）。名前指定は成功しないこと。
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
                    !r.success,
                    "撤去済み nostr_reply は fail-closed（publish 副経路を残さない）"
                );
            }
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(EngineResult {
                response: self.response.clone(),
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                last_posting_utterance_id: None,
                last_generation_had_continuation_speech: false,
                xml_fallback_parses: 0,
            })
        }

        fn build_agent_context(
            &self,
            _agent_id: &str,
            _caller: &CallerIdentity,
        ) -> (String, String) {
            ("base prompt".to_string(), "テストくん".to_string())
        }

        fn build_conversation_string(
            &self,
            _session_id: &str,
            _agent_id: &str,
            _budget: usize,
            _system_prompt: &str,
            _runtime_context_text: &str,
        ) -> anyhow::Result<String> {
            Ok("conversation".to_string())
        }

        fn context_budget_tokens(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _system_prompt: &str,
            _runtime_context_text: &str,
        ) -> Result<usize, opencrab_core::context_budget::ContextBudgetError> {
            Ok(1000)
        }

        fn has_llm_providers(&self) -> bool {
            true
        }

        fn agent_exists(&self, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        fn session_locks(&self) -> std::sync::Arc<opencrab_actions::SessionLocks> {
            self.session_locks.clone()
        }

        fn subtask_registry_for(&self, session_id: &str) -> opencrab_actions::SubtaskRegistry {
            opencrab_actions::SubtaskRegistries::new().registry_for(session_id)
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
        /// この sink は呼び出し元を**導出しない**（受け取るだけ）。解決の配線は
        /// 受信ループ側（`manager` のテスト）と server 側の実体でテストする。
        fn resolve_nostr_caller(&self, _agent_id: &str, _author_pubkey: &str) -> CallerIdentity {
            unreachable!("応答生成経路は呼び出し元を導出しない（引数で受け取る / #319）")
        }

        fn nostr_gate_allow_keys(
            &self,
            _agent_id: &str,
        ) -> anyhow::Result<crate::NostrGateAllowKeys> {
            // 応答生成 sink は元栓ゲート（受信ループ側）を通らないので使わない。
            unreachable!("応答生成経路は元栓の許可源を導出しない（#698 は受信ループ側）")
        }

        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn list_session_watches_for_agent(
            &self,
            _agent_id: &str,
        ) -> anyhow::Result<Vec<opencrab_db::queries::SessionWatchRow>> {
            Ok(Vec::new())
        }

        fn get_session_policy_json(&self, _session_id: &str) -> anyhow::Result<Option<String>> {
            Ok(Some("{}".to_string()))
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            None
        }

        fn set_nostr_secret_key(&self, _agent_id: &str, _secret_key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_nostr_self_pubkey(&self, _agent_id: &str, _self_pubkey: &str) -> anyhow::Result<()> {
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

        fn agent_workspace_root(&self, _agent_id: &str) -> Option<std::path::PathBuf> {
            None
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
            let script = crate::test_support::write_fake_nostaro(
                dir.path(),
                &format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
            );
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

    fn settled_with_caller(
        session_id: &str,
        reply_target: Option<&str>,
        caller: CallerIdentity,
    ) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-sink-test".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind: SettleKind::Completed,
            reply_target: reply_target.map(|s| s.to_string()),
            caller,
        }
    }

    /// 呼び出し元を指定しない既定（最小権限）の `settled`。
    fn settled(session_id: &str, reply_target: Option<&str>) -> SubtaskSettled {
        settled_with_caller(session_id, reply_target, CallerIdentity::Agent)
    }

