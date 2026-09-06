    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    use opencrab_actions::webhook_target::WebhookConfig;
    use opencrab_actions::{CallerIdentity, RunRequest};
    use opencrab_core::EngineResult;
    use opencrab_db::queries::AgentNostrConfigRow;

    /// #603: TimedFireRouter は `new` の必須引数になった。これらのテストは時刻発火を検証しない
    /// ので、空のルータを渡すだけの共通ヘルパを使う（構築子の記述量を増やさない）。
    fn test_router() -> Arc<opencrab_actions::TimedFireRouter> {
        Arc::new(opencrab_actions::TimedFireRouter::new())
    }

    /// 受信ループの非ブロック性・順序保証の検証用の最小 runner。LLM も DB も使わない。
    #[derive(Clone)]
    struct SlowRunner {
        delay: Duration,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
        /// 転記された受信メッセージ（順序の検証用）。
        recorded: Arc<Mutex<Vec<String>>>,
        /// 受信の転記先 session_id（session_id 規約の検証用 / #323）。
        recorded_sessions: Arc<Mutex<Vec<String>>>,
        /// 受信の発言者 id（1 セッションに混ざっても誰の発言か分かることの検証用 / #323）。
        recorded_speakers: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った session_id（#323）。
        run_sessions: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**開始**した順（reply_target）。
        started: Arc<Mutex<Vec<String>>>,
        /// 応答生成を**完了**した順（reply_target = 実際に返信が飛ぶ順）。
        finished: Arc<Mutex<Vec<String>>>,
        /// 転記先の解決結果（#252）。`None` = 未設定（転記しない）。
        relay_target: Option<WebhookConfig>,
        /// 実際に転記口へ渡った本文（配送口のスパイ / #252）。
        relayed: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った system_prompt（= base + prompt_suffix / #282）。
        system_prompts: Arc<Mutex<Vec<String>>>,
        /// upsert された agent_nostr_config 行（自己ブートストラップ採用の検証 / #264）。
        upserted: Arc<Mutex<Vec<AgentNostrConfigRow>>>,
        /// set_nostr_enabled の呼び出し履歴（順序ガードの検証 / #264）。
        enabled_calls: Arc<Mutex<Vec<bool>>>,
        /// set_nostr_secret_key に渡った nsec（ホットスワップ経路の検証 / #264）。
        secret_sets: Arc<Mutex<Vec<String>>>,
        /// set_nostr_self_pubkey に渡った pubkey（co_agent 逆引き表の書き戻し検証 / #489）。
        self_pubkey_sets: Arc<Mutex<Vec<String>>>,
        /// get_nostr_config が返す既存行（`None`=未設定 / #264）。
        preset_config: Option<AgentNostrConfigRow>,
        /// `resolve_nostr_caller` が Owner と答える相手の pubkey（#319）。
        owner_pubkey: Option<String>,
        /// #698: `nostr_gate_allow_keys` が返す trusted_users の pubkey（元栓の許可源検証）。
        trusted_pubkeys: Vec<String>,
        /// #698: `nostr_gate_allow_keys` が返す co_agent の pubkey（元栓の許可源検証）。
        co_agent_pubkeys: Vec<String>,
        /// #698: true なら `nostr_gate_allow_keys` が `Err`（DB 故障を模す。前回値保持の検証）。
        allow_keys_error: bool,
        /// `resolve_nostr_caller` に渡された pubkey（発言者を見ているかの検証 / #319）。
        caller_queries: Arc<Mutex<Vec<String>>>,
        /// 応答生成へ渡った呼び出し元（#319）。
        callers: Arc<Mutex<Vec<CallerIdentity>>>,
        /// `agent_workspace_root` が返す退避先（#570）。`None`＝退避先なし。
        workspace_root: Option<std::path::PathBuf>,
        /// #588 Stage 2: 1 つだけ保持し `session_locks()` は毎回この clone を返す
        /// （trait の「プロセス全体で 1 実体を共有」契約を fake でも守る）。
        session_locks: std::sync::Arc<opencrab_actions::SessionLocks>,
    }

    impl SlowRunner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                inflight: Arc::new(AtomicUsize::new(0)),
                max_inflight: Arc::new(AtomicUsize::new(0)),
                recorded: Arc::new(Mutex::new(Vec::new())),
                recorded_sessions: Arc::new(Mutex::new(Vec::new())),
                recorded_speakers: Arc::new(Mutex::new(Vec::new())),
                run_sessions: Arc::new(Mutex::new(Vec::new())),
                started: Arc::new(Mutex::new(Vec::new())),
                finished: Arc::new(Mutex::new(Vec::new())),
                relay_target: None,
                relayed: Arc::new(Mutex::new(Vec::new())),
                system_prompts: Arc::new(Mutex::new(Vec::new())),
                upserted: Arc::new(Mutex::new(Vec::new())),
                enabled_calls: Arc::new(Mutex::new(Vec::new())),
                secret_sets: Arc::new(Mutex::new(Vec::new())),
                self_pubkey_sets: Arc::new(Mutex::new(Vec::new())),
                preset_config: None,
                owner_pubkey: None,
                trusted_pubkeys: Vec::new(),
                co_agent_pubkeys: Vec::new(),
                allow_keys_error: false,
                caller_queries: Arc::new(Mutex::new(Vec::new())),
                callers: Arc::new(Mutex::new(Vec::new())),
                workspace_root: None,
                session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
            }
        }

        /// 受信本文の退避先を仕込む（#570 の退避経路の検証用）。
        fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
            self.workspace_root = Some(root);
            self
        }

        /// 「この pubkey がオーナー」という解決結果を仕込む（#319）。
        fn with_owner_pubkey(mut self, pubkey: &str) -> Self {
            self.owner_pubkey = Some(pubkey.to_string());
            self
        }

        /// #698: この pubkey を trusted_user（platform=nostr）として許可源に載せる。
        fn with_trusted_pubkey(mut self, pubkey: &str) -> Self {
            self.trusted_pubkeys.push(pubkey.to_string());
            self
        }

        /// #698: この pubkey を co_agent（owner 等価）として許可源に載せる。
        fn with_co_agent_pubkey(mut self, pubkey: &str) -> Self {
            self.co_agent_pubkeys.push(pubkey.to_string());
            self
        }

        /// #698: `nostr_gate_allow_keys` を `Err` にする（DB 故障の模擬。前回値保持の検証用）。
        fn with_allow_keys_error(mut self) -> Self {
            self.allow_keys_error = true;
            self
        }

        /// get_nostr_config が返す既存設定を仕込む（ホットスワップ経路の検証 / #264）。
        fn with_preset_config(mut self, row: AgentNostrConfigRow) -> Self {
            self.preset_config = Some(row);
            self
        }

        /// 転記先を有効化した runner（#252 のフック検証用）。
        fn with_relay_target(mut self, url: &str) -> Self {
            self.relay_target = Some(WebhookConfig {
                url: url.to_string(),
                events: None,
            });
            self
        }

        fn finished_len(&self) -> usize {
            self.finished.lock().unwrap().len()
        }

        fn snapshot(list: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
            list.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl opencrab_actions::AgentRuntime for SlowRunner {
        async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            let target = req.reply_target.clone().unwrap_or_default();
            self.callers.lock().unwrap().push(req.caller.clone());
            self.run_sessions
                .lock()
                .unwrap()
                .push(req.session_id.clone());
            self.system_prompts
                .lock()
                .unwrap()
                .push(req.system_prompt.clone());
            self.started.lock().unwrap().push(target.clone());
            let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, AtomicOrdering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
            self.finished.lock().unwrap().push(target);
            // NO_REPLY にして nostaro（外部プロセス）を一切呼ばない。
            Ok(EngineResult {
                response: "NO_REPLY".to_string(),
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
            ("base".to_string(), "テストくん".to_string())
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

        fn record_inbound_message(
            &self,
            source: opencrab_actions::TranscriptSource,
            record: &opencrab_actions::InboundMessageRecord<'_>,
        ) -> bool {
            assert_eq!(source, opencrab_actions::TranscriptSource::Nostr);
            self.recorded.lock().unwrap().push(record.text.to_string());
            self.recorded_sessions
                .lock()
                .unwrap()
                .push(record.session_id.to_string());
            self.recorded_speakers
                .lock()
                .unwrap()
                .push(record.sender_id.to_string());
            true
        }

        fn on_inbound_message(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _agent_id: &str,
            _record: &opencrab_actions::InboundMessageRecord<'_>,
        ) {
            // 受信フック（#156 S4）。Nostr の受信はまだ配線していないので no-op。
        }

        // 以下はこの経路が使わない（NO_REPLY で返すので応答転記も走らない）。
        fn record_outbound_reply(
            &self,
            _source: opencrab_actions::TranscriptSource,
            _record: &opencrab_actions::OutboundReplyRecord<'_>,
        ) {
            unimplemented!("nostr の fake は NO_REPLY を返すので応答転記を使わない")
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

    impl NostrAgentRunner for SlowRunner {
        /// 「オーナーの pubkey なら Owner」を模す（#319。解決の実体は server 側でテスト）。
        /// 問い合わせられた pubkey を記録して、**発言者を見ているか**を検証できるようにする。
        fn resolve_nostr_caller(&self, _agent_id: &str, author_pubkey: &str) -> CallerIdentity {
            self.caller_queries
                .lock()
                .unwrap()
                .push(author_pubkey.to_string());
            match self.owner_pubkey.as_deref() {
                Some(owner) if owner == author_pubkey => CallerIdentity::Owner,
                _ => CallerIdentity::Agent,
            }
        }

        /// #698: DB 由来の許可源を模す。owner / trusted / co_agent を仕込んだぶんだけ返す。
        /// `allow_keys_error` が立っていれば `Err`（DB 故障を模す）。
        fn nostr_gate_allow_keys(
            &self,
            _agent_id: &str,
        ) -> anyhow::Result<crate::NostrGateAllowKeys> {
            if self.allow_keys_error {
                anyhow::bail!("テスト: DB 故障を模した許可源の読み出し失敗");
            }
            Ok(crate::NostrGateAllowKeys {
                owner: self.owner_pubkey.clone().into_iter().collect(),
                co_agents: self.co_agent_pubkeys.clone(),
                trusted_users: self.trusted_pubkeys.clone(),
            })
        }

        fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow> {
            Vec::new()
        }

        fn get_nostr_config(&self, _agent_id: &str) -> Option<AgentNostrConfigRow> {
            self.preset_config.clone()
        }

        fn set_nostr_secret_key(&self, _a: &str, s: &str) -> anyhow::Result<()> {
            self.secret_sets.lock().unwrap().push(s.to_string());
            Ok(())
        }

        fn set_nostr_self_pubkey(&self, _a: &str, pk: &str) -> anyhow::Result<()> {
            self.self_pubkey_sets.lock().unwrap().push(pk.to_string());
            Ok(())
        }

        fn upsert_nostr_config(&self, cfg: &AgentNostrConfigRow) -> anyhow::Result<()> {
            self.upserted.lock().unwrap().push(cfg.clone());
            Ok(())
        }

        fn set_nostr_enabled(&self, _agent_id: &str, enabled: bool) -> anyhow::Result<()> {
            self.enabled_calls.lock().unwrap().push(enabled);
            Ok(())
        }

        fn resolve_nostr_relay_target(&self, _agent_id: &str) -> Option<WebhookConfig> {
            self.relay_target.clone()
        }

        fn relay_inbound_notification(&self, _target: &WebhookConfig, text: String) {
            // 配送口のスパイ: 実際に転記へ回った本文を記録する（HTTP は出さない）。
            self.relayed.lock().unwrap().push(text);
        }

        fn agent_workspace_root(&self, _agent_id: &str) -> Option<std::path::PathBuf> {
            self.workspace_root.clone()
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

    fn event(id: &str, pubkey: &str, content: &str) -> NostrEvent {
        NostrEvent {
            id: id.to_string(),
            pubkey: pubkey.to_string(),
            npub: None,
            note_id: Some(format!("note1{id}")),
            author_name: None,
            created_at: 0,
            kind: 1,
            content: content.to_string(),
            tags: Vec::new(),
        }
    }

    /// 受信ループ相当の呼び出しを組み立てるテスト用ハーネス。
    struct Harness {
        runner: SlowRunner,
        admin: Arc<dyn NostrIdentityAdmin>,
        runtime: Arc<NostrSessionRuntime>,
        permits: Arc<Semaphore>,
        queues: Arc<SessionQueues>,
        cli: NostaroCli,
        agent_id: String,
        /// #698 元栓の許可集合。owner / co_agent / trusted_users は構築時に runner の
        /// `nostr_gate_allow_keys` から載せる。`feed*` は流す前に発言者を followees へ入れる
        /// （既存テストはフォロー元栓を検証対象にしていないので素通しを保つ）。ゲート自体の
        /// 検証は [`Self::feed_unfollowed_event`]（followees に入れない経路）で行う。
        allow: AllowGate,
        /// #698 元栓で捨てた件数（揮発カウンタ）。
        dropped: Arc<AtomicU64>,
    }

    impl Harness {
        fn new(agent_id: &str, delay: Duration, permits: usize, capacity: usize) -> Self {
            Self::with_runner(agent_id, SlowRunner::new(delay), permits, capacity)
        }

        fn with_runner(
            agent_id: &str,
            runner: SlowRunner,
            permits: usize,
            capacity: usize,
        ) -> Self {
            // 本番の build_allow_sources と同じく、DB 由来キー（owner/co_agent/trusted）を
            // follow_key で寄せて許可集合に載せる。followees は feed 時に足す。
            let db = runner.nostr_gate_allow_keys(agent_id).expect(
                "テスト: 許可源の取得（allow_keys_error を立てた runner は Harness に使わない）",
            );
            let to_set = |v: &[String]| -> HashSet<String> {
                v.iter().map(|s| crate::pubkey::follow_key(s)).collect()
            };
            let allow = Arc::new(RwLock::new(AllowSources {
                followees: HashSet::new(),
                owner: to_set(&db.owner),
                co_agents: to_set(&db.co_agents),
                trusted_users: to_set(&db.trusted_users),
            }));
            Self {
                runner,
                admin: Arc::new(NoopAdmin),
                runtime: Arc::new(NostrSessionRuntime::new()),
                permits: Arc::new(Semaphore::new(permits)),
                queues: Arc::new(SessionQueues::new(capacity)),
                cli: NostaroCli::new(),
                agent_id: agent_id.to_string(),
                allow,
                dropped: Arc::new(AtomicU64::new(0)),
            }
        }

        /// watch ループが 1 行読んだのと同じ処理（同期・await 無し）。
        async fn feed(&self, id: &str, pubkey: &str, content: &str) {
            self.feed_event(event(id, pubkey, content)).await;
        }

        /// 任意のイベントを1件流す（メタ情報の検証用 / #282）。
        async fn feed_event(&self, ev: NostrEvent) {
            self.feed_event_as(&self.agent_id, ev).await;
        }

        /// **別エージェント**として1件流す（= 別セッション / #323）。
        ///
        /// session が agent 単位になったので、「別セッション」を作る唯一の軸が
        /// エージェントになった。本番では `permits` / `queues` はエージェント毎に
        /// 作られるが（[`run_nostr_loop`]）、ここで見たいのは [`SessionQueues`] が
        /// 複数 session を持ったときの挙動なので、意図的に 1 束を共有して流す。
        ///
        /// #698: 発言者を followees へ入れてから流す（＝フォロイー扱い）。フォロー元栓を
        /// 検証しない既存テストの素通しを保つため。ゲートの検証は [`Self::feed_unfollowed_event`]。
        async fn feed_event_as(&self, agent_id: &str, ev: NostrEvent) {
            self.allow
                .write()
                .unwrap()
                .followees
                .insert(crate::pubkey::follow_key(&ev.pubkey));
            self.dispatch(agent_id, ev).await;
        }

        /// 許可集合へ入れずに1件流す（元栓の検証用 / #698）。発言者がフォロイー・owner・
        /// co_agent・trusted_user のいずれでもなければ、ゲートで捨てられる。
        async fn feed_unfollowed_event(&self, ev: NostrEvent) {
            self.dispatch(&self.agent_id, ev).await;
        }

        /// `handle_event` を現在の allow / dropped で呼ぶ共通経路。
        async fn dispatch(&self, agent_id: &str, ev: NostrEvent) {
            handle_event(
                &self.runner,
                &self.cli,
                agent_id,
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                &self.allow,
                &self.dropped,
                &self.admin,
                &self.runtime,
                &self.permits,
                &self.queues,
                ev,
            )
            .await;
        }

        fn dropped(&self) -> u64 {
            self.dropped.load(AtomicOrdering::SeqCst)
        }

        /// [`Self::feed`] の別エージェント版（#323）。
        async fn feed_as(&self, agent_id: &str, id: &str, pubkey: &str, content: &str) {
            self.feed_event_as(agent_id, event(id, pubkey, content))
                .await;
        }

        /// 応答生成が `n` 件完了するまで待つ（タイムアウトしたら false）。
        async fn wait_finished(&self, n: usize, timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                if self.runner.finished_len() >= n {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            self.runner.finished_len() >= n
        }
    }

