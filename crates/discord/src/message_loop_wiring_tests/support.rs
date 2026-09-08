/// `run_agent_response` の観測 1 件。
///
/// `subtask_registry` を **`Option<SubtaskRegistry>` のまま**保持するのが要点。
/// bool に潰すと同一性の検査ができない。
struct RunObservation {
    session_id: String,
    subtask_registry: Option<SubtaskRegistry>,
    has_completion_sink: bool,
    /// この run の呼び出し元（#298）。resume が元の権限を落としていないことの検査に使う。
    caller: CallerIdentity,
    /// 「発言終わり」🏁 判定用の subtask 起動カウンタ（#431）。**`Option` のまま**
    /// 保持する（bool に潰すと未配線を検出できるだけで、実体の共有は見られない）。
    subtask_starts: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// 受信フック（`AgentRuntime::on_inbound_message`）の観測 1 件。
///
/// 回収の中身（`[Peer Review]` の解析・ゲート）は汎用層のテストが持つ。ここで固定
/// するのは**受信ループがフックを呼ぶこと**と、そのとき渡す由来・帰属・本文。
#[derive(Debug, Clone)]
struct InboundHookCall {
    source: opencrab_actions::TranscriptSource,
    agent_id: String,
    session_id: String,
    sender_id: String,
    sender_name: String,
    text: String,
}

/// テスト用の最小 `AgentRunner`。LLM も Discord API も叩かず、応答は**空**を返す
/// （空応答は送信経路に入らないので、テストがネットワークへ出ない）。
#[derive(Clone)]
struct FakeRunner {
    runs: Arc<Mutex<Vec<RunObservation>>>,
    /// 受信フックの観測（呼ばれた順）。
    inbound_hooks: Arc<Mutex<Vec<InboundHookCall>>>,
    /// 受信発言の**記録**（`record_inbound_message`）の観測（本文、呼ばれた順）。
    /// フック（`on_inbound_message`）とは別物で、こちらが会話履歴に残る本体。
    inbound_records: Arc<Mutex<Vec<String>>>,
    /// `record_inbound_message` が false（記録失敗）を返すよう強制するフラグ。
    inbound_record_fails: Arc<std::sync::atomic::AtomicBool>,
    /// run を 1 件観測したことの通知。
    ///
    /// inbound 経路の応答生成は `SessionLocks::spawn_serialized` の別タスクで走るため、
    /// 呼び出しから戻った時点ではまだ観測されていない。ポーリングで待つと「上限内に
    /// 走らなかった」だけで落ちる（負荷の高い CI で偽陽性）ので、通知で待つ。
    /// `notify_one` は待ち手が居なくても permit を 1 つ残すため、`notified()` を
    /// 後から await しても取りこぼさない。
    run_observed: Arc<tokio::sync::Notify>,
    db: opencrab_db::Db,
    /// #588 Stage 2: 1 つだけ保持し `session_locks()` は毎回この clone を返す
    /// （trait の「プロセス全体で 1 実体を共有」契約を fake でも守る）。
    session_locks: std::sync::Arc<opencrab_actions::SessionLocks>,
    /// チャンネル whitelist。row 116-117 の「落ちた件に 👀 なし」ピン用。
    channel_whitelisted: bool,
}

impl FakeRunner {
    fn new() -> Self {
        Self {
            runs: Arc::new(Mutex::new(Vec::new())),
            inbound_hooks: Arc::new(Mutex::new(Vec::new())),
            inbound_records: Arc::new(Mutex::new(Vec::new())),
            inbound_record_fails: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            run_observed: Arc::new(tokio::sync::Notify::new()),
            db: opencrab_db::Db::memory().expect("in-memory DB"),
            session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
            channel_whitelisted: true,
        }
    }

    /// run が 1 件観測されるまで待つ。上限は「壊れたときに無限に吊らない」ための保険
    /// であって、正常系の待ち時間ではない（通知が来た瞬間に戻る）。
    async fn wait_for_run(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.run_observed.notified(),
        )
        .await
        .expect("応答生成が走らなかった（run が 1 件も観測されていない）");
    }

    /// run が `n` 件観測されるまで待つ。複数グループ（連続同権限グループ化 #556）で run が
    /// 複数起きるケースの検証に使う。`notify_one` の permit は 1 つしか溜まらないため、
    /// 取りこぼしても 100ms ごとにポーリングして件数で確認する（上限は無限吊り防止の保険）。
    async fn wait_for_runs(&self, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if self.runs.lock().unwrap().len() >= n {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "run が {n} 件に達しなかった（観測 {} 件）",
                    self.runs.lock().unwrap().len()
                );
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                self.run_observed.notified(),
            )
            .await;
        }
    }

    /// 観測した run の登録簿 + sink 有無を取り出す。
    fn observed(&self, index: usize) -> (String, Option<SubtaskRegistry>, bool) {
        let runs = self.runs.lock().unwrap();
        let r = runs.get(index).expect("run が観測されていない");
        (
            r.session_id.clone(),
            r.subtask_registry.clone(),
            r.has_completion_sink,
        )
    }

    /// 観測した run の subtask 起動カウンタ（#431）。
    fn observed_subtask_starts(&self, index: usize) -> Option<Arc<std::sync::atomic::AtomicUsize>> {
        let runs = self.runs.lock().unwrap();
        runs.get(index)
            .expect("run が観測されていない")
            .subtask_starts
            .clone()
    }

    /// 観測した run の呼び出し元（#298）。
    fn observed_caller(&self, index: usize) -> CallerIdentity {
        let runs = self.runs.lock().unwrap();
        runs.get(index)
            .expect("run が観測されていない")
            .caller
            .clone()
    }
}

#[async_trait::async_trait]
impl opencrab_actions::AgentRuntime for FakeRunner {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.runs.lock().unwrap().push(RunObservation {
            session_id: req.session_id.clone(),
            subtask_registry: req.subtask_registry.clone(),
            has_completion_sink: req.completion_sink.is_some(),
            caller: req.caller.clone(),
            subtask_starts: req.subtask_starts.clone(),
        });
        self.run_observed.notify_one();
        // 空応答: 転記も Discord 送信も走らない（この fake はネットワークへ出ない）。
        Ok(EngineResult {
            response: String::new(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit: false,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        })
    }

    fn build_agent_context(&self, _agent_id: &str, _caller: &CallerIdentity) -> (String, String) {
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

    fn record_inbound_message(
        &self,
        _source: opencrab_actions::TranscriptSource,
        record: &opencrab_actions::InboundMessageRecord<'_>,
    ) -> bool {
        self.inbound_records
            .lock()
            .unwrap()
            .push(record.text.to_string());
        !self
            .inbound_record_fails
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn on_inbound_message(
        &self,
        source: opencrab_actions::TranscriptSource,
        agent_id: &str,
        record: &opencrab_actions::InboundMessageRecord<'_>,
    ) {
        self.inbound_hooks.lock().unwrap().push(InboundHookCall {
            source,
            agent_id: agent_id.to_string(),
            session_id: record.session_id.to_string(),
            sender_id: record.sender_id.to_string(),
            sender_name: record.sender_name.to_string(),
            text: record.text.to_string(),
        });
    }

    fn record_outbound_reply(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _record: &opencrab_actions::OutboundReplyRecord<'_>,
    ) {
    }

    fn record_interaction_response(
        &self,
        _agent_id: &str,
        _session_id: &str,
        _record: &opencrab_actions::InteractionRecord<'_>,
    ) {
        // 記録の中身はこのファイルの検査対象ではない（#298 では resume の権限だけを見る）。
    }

    fn ensure_session(&self, _s: &str, _a: &[String], _t: &str, _m: &str, _mode: &str) {}

    fn session_theme(&self, _session_id: &str) -> Option<String> {
        Some("Subtask: ダミー作業".to_string())
    }

    fn mark_interaction_status(
        &self,
        _interaction_id: &str,
        _status: &str,
        _response_json: Option<&str>,
        _responder_id: Option<&str>,
    ) {
        // 同上（#298）。
    }

    fn cleanup_stale_interactions(&self) {
        unimplemented!("この fake は起動時掃除を使わない")
    }

    fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {
        unimplemented!("この fake は起動時掃除を使わない")
    }
}

impl crate::AgentRunner for FakeRunner {
    fn db(&self) -> &opencrab_db::Db {
        &self.db
    }

    fn workspace_base(&self) -> &str {
        "/nonexistent/workspace/{agent_id}"
    }

    fn is_channel_writable(&self, _channel_id: &str) -> bool {
        // 空応答なので送信判定までは来ないが、来ても書き込まない側に倒す。
        false
    }

    fn is_channel_whitelisted_for_agent(&self, _channel_id: &str, _agent_id: &str) -> bool {
        self.channel_whitelisted
    }

    fn dm_allowed_any(
        &self,
        _sender_id: &str,
        _agent_ids: &[String],
        _owner_discord_id: &str,
    ) -> bool {
        true
    }

    fn dm_allowed(&self, _sender_id: &str, _agent_id: &str, _owner_discord_id: &str) -> bool {
        true
    }

    fn resolve_caller(
        &self,
        sender_id: &str,
        _agent_ids: &[String],
        owner_discord_id: &str,
    ) -> CallerIdentity {
        // 実装に忠実に: 送信者がオーナー本人なら Owner、それ以外は TrustedUser。
        // #556 で「権限レベルが違っても同じ channel 窓に合流する」ことの検証に使う
        // （owner=rank2 と非owner=rank1 を混在させて run が 1 回になることを確認する）。
        if sender_id == owner_discord_id {
            CallerIdentity::Owner
        } else {
            CallerIdentity::TrustedUser
        }
    }

    fn list_enabled_discord_configs(&self) -> Vec<opencrab_db::queries::AgentDiscordConfigRow> {
        Vec::new()
    }

    fn get_discord_config(
        &self,
        _agent_id: &str,
    ) -> Option<opencrab_db::queries::AgentDiscordConfigRow> {
        None
    }

    fn served_by_dedicated_gateway(&self, _agent_id: &str) -> bool {
        false
    }
}

/// 本番と同じ形の依存一式。
///
/// `DiscordGateway::new` は HTTP クライアントとチャンネルを組むだけで接続しないが、
/// `process_incoming_message` まで通すテストでは typing / リアクションの送信が
/// discord.com へ出て 401 になる（テストはその失敗ログを観測する）。
fn make_deps() -> (
    FakeRunner,
    Arc<DiscordGateway>,
    Arc<dyn opencrab_gateway::GatewayActions>,
) {
    let state = FakeRunner::new();
    let gateway = Arc::new(DiscordGateway::new("test-token"));
    let actions = crate::DiscordGatewayActions::new(
        gateway.http().clone(),
        state.db.clone(),
        "/nonexistent/workspace/{agent_id}".to_string(),
        None,
    );
    (state, gateway, Arc::new(actions))
}
