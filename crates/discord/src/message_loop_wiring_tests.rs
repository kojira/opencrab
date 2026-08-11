//! 本番配線の同一性テスト（#203 要ビルド検証リスト 3）。
//!
//! 「非ブロック dispatch を有効にしたか」（`completion_sink.is_some()` 等の bool）だけを
//! 見るテストでは、**別インスタンス**の登録簿を run へ渡す壊れ方を検出できない。
//! `cancel_subtask` は「そのセッションの登録簿から subtask を引く」実装なので、run に
//! 別の登録簿が載ると auto-dispatch した subtask が永久に停止できなくなる（Discord の
//! `cancel_subtask` が常に "not found" を返す）。ここでは `Arc::ptr_eq` で
//! **同一実体**であることを固定する。
//!
//! 形は `crates/web-gateway/src/respond.rs` の
//! `run_uses_the_gateways_registry_so_cancel_can_reach_it` に倣う。
//!
//! ファイルを分けている理由: `message_loop.rs` へ変異（mutation）を入れて
//! `git checkout -- crates/discord/src/message_loop.rs` で戻すとき、テストごと
//! 巻き戻さないようにするため。
//!
//! **未固定の範囲**: `handle_agent_response` を直接呼ぶテスト（NO_REPLY の 🤐）は、
//! 呼び出し側が渡す引数（`&discord_message_id_spawn` を `""` に差し替える／`channel_id`
//! を取り違える等）の変異を検出できない。**配線側は未固定。実機確認で補う。**

use std::sync::{Arc, Mutex};

use opencrab_actions::subtask::SubtaskRegistry;
use opencrab_actions::{CallerIdentity, RunRequest, SessionLocks};
use opencrab_core::EngineResult;
use opencrab_gateway::{DiscordGateway, IncomingMessage, MessageContent, MessageSource, Sender};

use super::{
    consecutive_groups, create_event_channel, debounce_window_key, incoming_has_content,
    process_incoming_message, process_interaction_response, process_subtask_completed,
};

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
    ) -> anyhow::Result<String> {
        Ok("conversation".to_string())
    }

    fn context_budget_tokens(&self, _agent_id: &str) -> usize {
        1000
    }

    fn has_llm_providers(&self) -> bool {
        true
    }

    fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {}

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
        true
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

/// **resume（subtask 完了）の run に載る登録簿は、呼び出し側が渡した実体そのもので
/// なければならない。**
///
/// `cancel_subtask` はセッションの登録簿から subtask を引くので、resume 実行に別の
/// 登録簿を渡すと、その run が auto-dispatch した subtask はどこからも停止できない
/// （Discord の `cancel_subtask` が常に "not found" を返す）。dispatch が有効か
/// （bool）だけの検査では別実体の取り違えを検出できないため、`Arc::ptr_eq` で固定する。
#[tokio::test]
async fn resume_run_carries_the_caller_registry_so_cancel_can_reach_it() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果本文".to_string(),
        "completed".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry.clone(),
        opencrab_actions::CallerIdentity::Agent,
    )
    .await;

    let (session_id, observed, has_sink) = state.observed(0);
    assert_eq!(session_id, "discord-crab-111-222");
    let observed = observed.expect("run に登録簿が載っていない（非ブロック実行が無効）");
    assert!(
        Arc::ptr_eq(&observed, &registry),
        "resume の応答生成に渡した登録簿が、停止処理が引くものと別インスタンスになっている"
    );
    assert!(
        has_sink,
        "resume の run に完了 sink が無い（掘削の完了が再注入されない）"
    );
}

/// **inbound（Discord 受信）の run に載る登録簿も、ループが持つ共有実体そのもので
/// なければならない。**
///
/// こちらが本番の主経路。ここで別の登録簿を渡すと、通常の会話から auto-dispatch した
/// background subtask が `cancel_subtask` の到達範囲から外れて停止不能になる。
#[tokio::test]
async fn inbound_run_carries_the_shared_registry_so_cancel_can_reach_it() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("掘削して".to_string()),
        Sender::new("user-1", "だれか"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry.clone(),
        false,
    )
    .await;

    // 応答生成は `SessionLocks::spawn_serialized` の中で走るので、観測の通知を待つ。
    state.wait_for_run().await;

    let (session_id, observed, has_sink) = state.observed(0);
    assert_eq!(session_id, "discord-crab-111-222");
    let observed = observed.expect("run に登録簿が載っていない（非ブロック実行が無効）");
    assert!(
        Arc::ptr_eq(&observed, &registry),
        "inbound の応答生成に渡した登録簿が、停止処理が引くものと別インスタンスになっている"
    );
    assert!(
        has_sink,
        "inbound の run に完了 sink が無い（掘削の完了が再注入されない）"
    );
}

/// **inbound の run は「発言終わり」🏁 判定用の subtask 起動カウンタを載せる**（#431）。
///
/// このカウンタが未配線（`None`）だと、run 側（auto-dispatch / 明示 `spawn_subtask`）が
/// 加算する先を失い、ゲートは常に「subtask を起こしていない」と見る。結果、掘削を
/// 投げたターンに 🏁 が付いて『調べますね🏁』の数分後に続きが届く逆情報に戻る。
/// **配線を落としても他のテストは落ちない**（ゲートの単体テストはカウンタの値を
/// 直接与えるため）ので、ここで配線そのものを固定する。
#[tokio::test]
async fn inbound_run_carries_the_end_of_speech_subtask_counter() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("掘削して".to_string()),
        Sender::new("user-1", "だれか"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry,
        false,
    )
    .await;

    state.wait_for_run().await;

    let counter = state
        .observed_subtask_starts(0)
        .expect("inbound の run に subtask 起動カウンタが載っていない（🏁 の判定が効かない）");
    // ターンごとに新しいカウンタで、run に入る時点では 0（前のターンの掘削を
    // 引きずらない）。
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "ターン開始時のカウンタは 0 でなければならない"
    );
}

/// **subtask 完了 resume の run にもカウンタが載る**（#431）。
///
/// resume ターンが**さらに**掘削を投げたときに、その resume へ 🏁 を付けず次の resume
/// へ委ねるための配線。ここが落ちると鎖の途中に印が付く。
#[tokio::test]
async fn resume_run_carries_the_end_of_speech_subtask_counter() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果".to_string(),
        "completed".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry,
        CallerIdentity::Owner,
    )
    .await;

    let counter = state
        .observed_subtask_starts(0)
        .expect("resume の run に subtask 起動カウンタが載っていない");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// **Discord の受信は共通の受信フックを必ず通る**（#156 S4）。
///
/// ピアレビュー返信の回収は以前 Discord の受信ループが直接呼ぶ専用関数だった。汎用層へ
/// 移した後にこの呼び出しが落ちると、回収は**静かに止まる**（返信は普通の発言として
/// 流れるだけなのでログにも異常が出ない）。ここでフックの呼び出しと、渡す由来・帰属・
/// 本文を固定する。回収そのもののゲートは汎用層（`crates/server/src/peer_review.rs`）の
/// テストが持つ。
#[tokio::test]
async fn inbound_goes_through_the_shared_inbound_hook() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    let reply = "[Peer Review] score: 0.7, gaps: none, summary: ok";
    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text(reply.to_string()),
        Sender::new("user-1", "crab-b"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry,
        false,
    )
    .await;

    // フックは応答生成と同じ直列タスクの中（会話組み立ての前）で呼ばれる。
    state.wait_for_run().await;

    let hooks = state.inbound_hooks.lock().unwrap();
    let call = hooks
        .first()
        .expect("受信が共通フック（on_inbound_message）を通っていない — 返信の回収が死ぬ");
    assert_eq!(call.source, opencrab_actions::TranscriptSource::Discord);
    // 帰属は**受信側エージェント**（誰の台帳に回収するか）。送信者と取り違えない。
    assert_eq!(call.agent_id, "crab");
    assert_eq!(call.session_id, "discord-crab-111-222");
    assert_eq!(call.sender_id, "user-1");
    assert_eq!(call.sender_name, "crab-b");
    assert_eq!(call.text, reply);
}

/// **ユーザー発言の記録はセッションロックの外（＝ロック待ちより前）で確定する。**（#284 P0-1）
///
/// 以前は `spawn_serialized` の内側で記録していたため、そのセッションで長い推論が
/// 走っている間に届いた発言は、推論が終わるまで DB に入らなかった。その窓でプロセスが
/// 落ちる／タスクが失われると**発言が永久に消える**（実際に起きた #284 の症状）。
///
/// ここでは同一セッションのロックをテスト側で握ったまま `process_incoming_message` を
/// 呼び、**ロックを握ったままでも記録が済んでいる**ことを固定する。実装をロックの
/// 内側へ戻すと、記録がロック解放待ちになりこのテストが落ちる。
#[tokio::test]
async fn inbound_message_is_recorded_before_the_session_lock_is_acquired() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());
    let session_id = "discord-crab-111-222";

    // 同一セッションのロックを掴んだまま離さないタスク（＝走行中の長い推論の代役）。
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let holder_locks = session_locks.clone();
    let holder = tokio::spawn(async move {
        holder_locks
            .run_serialized(session_id, async move {
                let _ = held_tx.send(());
                let _ = release_rx.await;
            })
            .await;
    });
    held_rx.await.expect("ロックが取得されなかった");

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("全員フォローして".to_string()),
        Sender::new("user-1", "owner"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry,
        false,
    )
    .await;

    // ロックはまだ握られている（応答生成は 1 件も走れていない）。
    assert!(
        state.runs.lock().unwrap().is_empty(),
        "テストの前提が崩れている: ロックを握ったままなのに応答生成が走った"
    );
    // それでも発言は記録済みでなければならない。
    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records,
        vec!["全員フォローして".to_string()],
        "ユーザー発言がセッションロックの解放待ちになっている（ロック中に失うと消える）"
    );

    let _ = release_tx.send(());
    holder.await.unwrap();
}

/// **記録に失敗したら黙って進まない。**（#284 P0-3）
///
/// `record_inbound_message` は best-effort ではなく成否を返す。false を無視すると、
/// エージェントはその発言を一度も見ないまま応答する（＝ #284 の症状そのもの）。
/// 呼び出し側が戻り値を評価していることを固定する（評価していなければ `#[must_use]`
/// と警告で気づけるが、警告はビルド設定で消せるのでテストでも縛る）。
#[test]
fn failed_inbound_record_is_detected_not_swallowed() {
    // `captured_logs` はスレッドローカルの捕捉先に依存するので、`#[tokio::test]` ではなく
    // 「捕捉クロージャの中で current-thread ランタイムを回す」形にする（同じスレッドで
    // 走らせないと warn を拾えない）。
    let logs = crate::owner_warning::capture::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (state, gateway, gateway_actions) = make_deps();
            state
                .inbound_record_fails
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let (event_tx, _event_rx) = create_event_channel();
            let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
            let session_locks = Arc::new(SessionLocks::new());

            let incoming = IncomingMessage::new(
                MessageSource::Discord {
                    guild_id: "111".to_string(),
                    channel_id: "222".to_string(),
                },
                MessageContent::Text("つらい".to_string()),
                Sender::new("user-1", "owner"),
            );

            process_incoming_message(
                incoming,
                gateway,
                state.clone(),
                vec!["crab".to_string()],
                gateway_actions,
                "owner-1".to_string(),
                session_locks,
                false,
                None,
                event_tx,
                registry,
                false,
            )
            .await;

            // 記録は試みられている（＝呼び出し自体は消えていない）。
            assert_eq!(state.inbound_records.lock().unwrap().len(), 1);
        });
    });

    // #286: 「呼ばれたこと」だけを見るとトートロジーになる。**false を受けて実際に
    // エスカレーションが出る**ところまで検査する（戻り値を捨てる実装に戻ると落ちる）。
    assert!(
        logs.contains("failed to persist an inbound user message"),
        "記録失敗が握り潰されている（警告が出ていない）: {logs}"
    );
    assert!(
        logs.contains("discord-crab-111-222"),
        "どのセッションで落ちたか分からない: {logs}"
    );
}

// ================================================================================
// #298: resume で呼び出し元（CallerIdentity）を落とさない
//
// `policy_allows`（`crates/actions/src/bridge.rs`）は owner_only / trusted_only の
// ツールを **list_tools からも dispatch からも** 落とす。resume の RunRequest を
// `CallerIdentity::Agent` 固定で組むと、オーナー発のターンが subtask 決着の瞬間に
// 降格し、owner/trusted のツールが丸ごと消える（`report_progress` を呼ぶと自分の
// 権限が落ちる、という自爆的な挙動）。
// ================================================================================

/// subtask 完了 resume は、subtask を spawn した元ターンの呼び出し元を保つ。
#[tokio::test]
async fn subtask_resume_preserves_the_original_caller() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        "結果本文".to_string(),
        "progress".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry,
        CallerIdentity::Owner,
    )
    .await;

    assert_eq!(
        state.observed_caller(0),
        CallerIdentity::Owner,
        "オーナー発のターンが subtask 決着で降格している（owner/trusted のツールが消える）"
    );
}

/// 昇格はしない: 元が `Agent` のターンは resume でも `Agent` のまま。
#[tokio::test]
async fn subtask_resume_keeps_agent_turns_as_agent() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    process_subtask_completed(
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        "st-1".to_string(),
        String::new(),
        "completed".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        false,
        gateway,
        state.clone(),
        gateway_actions,
        None,
        event_tx,
        registry,
        CallerIdentity::Agent,
    )
    .await;

    assert_eq!(
        state.observed_caller(0),
        CallerIdentity::Agent,
        "resume が権限の昇格経路になってはならない"
    );
}

/// A2UI 応答の resume を 1 回走らせ、`RunRequest` に載った呼び出し元を返す。
///
/// 引き継ぐのは**その UI を描いた run の呼び出し元**（`PendingInteraction.caller`）で、
/// 応答した本人（`responder_id`）からは導出しない（#302）。`send_ui` の `channel_id` は
/// 自由引数で、描画先チャンネルと resume 先セッション（`ctx.session_id`）は独立して
/// いるため、応答者から導くと「`Agent` のターンがオーナーの見るチャンネルへ UI を描き、
/// オーナーが押した瞬間にそのセッションが `Owner` で resume する」＝昇格経路になる。
/// クリックは `handle_component_interaction` の owner-only ゲートで既にオーナー限定
/// なので、応答者から導く実利も無い。
async fn interaction_resume_caller(caller: CallerIdentity) -> CallerIdentity {
    let (state, gateway, gateway_actions) = make_deps();

    process_interaction_response(
        "int-1".to_string(),
        "discord-crab-111-222".to_string(),
        "crab".to_string(),
        222,
        "222".to_string(),
        "111".to_string(),
        opencrab_core::a2ui::A2uiUserAction {
            surface_id: "s-1".to_string(),
            component_id: "btn-1".to_string(),
            action_name: "approve".to_string(),
            context: None,
            // 押せるのはオーナーだけ（owner-only ゲート）。この fake の
            // `resolve_caller` は誰であれ `TrustedUser` を返すので、応答者から
            // 導出する実装に戻すと下の 2 本が両方落ちる。
            responder_id: "owner-1".to_string(),
        },
        false,
        gateway,
        state.clone(),
        gateway_actions,
        caller,
    )
    .await;

    state.observed_caller(0)
}

/// 降格しない: 元がオーナー発のターンなら resume も `Owner`。
#[tokio::test]
async fn interaction_response_resume_preserves_the_drawing_run_caller() {
    assert_eq!(
        interaction_resume_caller(CallerIdentity::Owner).await,
        CallerIdentity::Owner,
        "UI 応答の resume が最小権限へ降格している（owner/trusted のツールが消える）"
    );
}

/// 昇格しない: 元が `Agent` のターンが描いた UI は、**オーナーが押しても** `Agent` のまま。
#[tokio::test]
async fn interaction_response_resume_does_not_escalate_agent_turns() {
    assert_eq!(
        interaction_resume_caller(CallerIdentity::Agent).await,
        CallerIdentity::Agent,
        "UI 応答の resume が権限の昇格経路になってはならない"
    );
}

// ---- NO_REPLY の可視化（#317） ----

/// リアクション付与だけを観測する fake（Discord へは出ない）。
#[derive(Default)]
struct FakeReactionGateway {
    /// (channel_id, message_id, emoji) を呼ばれた順に記録する。
    calls: Mutex<Vec<(u64, u64, String)>>,
}

#[async_trait::async_trait]
impl super::ReactionAdder for FakeReactionGateway {
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((channel_id, message_id, emoji.to_string()));
        Ok(())
    }
}

fn engine_result(response: &str) -> EngineResult {
    EngineResult {
        response: response.to_string(),
        iterations: 1,
        tool_calls_made: 0,
        stopped_by_limit: false,
        xml_fallback_parses: 0,
    }
}

async fn no_reply_reaction_calls(response: &str, message_id: &str) -> Vec<(u64, u64, String)> {
    let state = FakeRunner::new();
    let gateway = FakeReactionGateway::default();

    super::handle_agent_response(
        Ok(engine_result(response)),
        "crab",
        "discord-crab-111-222",
        222,
        "222",
        &state,
        &gateway,
        message_id,
    )
    .await;

    let calls = gateway.calls.lock().unwrap();
    calls.clone()
}

/// **`NO_REPLY` を選んだターンは、元の投稿にリアクションが付く。**
///
/// 付かないと、投稿者からは「読んで黙った」のか「落ちて返せなかった」のか区別が
/// つかない（これが #317 の要望そのもの）。宛先（チャンネル・メッセージ）と絵文字まで
/// 固定する — 宛先を取り違えると無関係な投稿にリアクションが付く。
#[tokio::test]
async fn no_reply_marks_the_original_message_with_a_reaction() {
    let calls = no_reply_reaction_calls("NO_REPLY", "1234567890123456789").await;
    assert_eq!(
        calls.len(),
        1,
        "NO_REPLY なのにリアクションが付いていない（黙ったことが誰にも見えない）"
    );
    assert_eq!(calls[0].0, 222, "リアクション先のチャンネルが違う");
    assert_eq!(
        calls[0].1, 1234567890123456789,
        "リアクション先のメッセージが違う"
    );
    // 👀（受け取った）と同じ絵文字にすると 2 つの状態が区別できなくなる。
    assert_eq!(calls[0].2, "🤐", "NO_REPLY の絵文字が変わっている");
    assert_ne!(calls[0].2, "👀", "受信済みマークと同じ絵文字になっている");
}

/// **普通に返答したターンにはリアクションを付けない。**
///
/// 返答があるのに「黙った」マークが付くと意味が反転する。
#[tokio::test]
async fn a_normal_reply_gets_no_no_reply_reaction() {
    let calls = no_reply_reaction_calls("ふつうの返事", "1234567890123456789").await;
    assert!(
        calls.is_empty(),
        "返答したターンに NO_REPLY のリアクションが付いている"
    );
}

/// **どんな送信者の投稿にも 👀 が付く**（#317: bot を特別扱いしない）。
///
/// 以前は `reaction_added` を「送信者が bot か」で初期化していたため、
/// 他エージェントの投稿には 👀 が付かなかった（＝エージェント同士の会話で
/// 「受け取った」が見えない）。無限ループを止めるのは**自分自身の投稿の除外**
/// （`crates/gateway/src/adapters/discord.rs` の `is_own_message`）であって、
/// bot フラグではない。
///
/// 観測は warn ログで行う。ここは本物の `DiscordGateway`（`test-token`）なので
/// 付与は必ず失敗するが、**付与を試みたこと**（＝配線が生きていること）はログに残る。
/// 付与をスキップした場合は絵文字がログに一切現れない。
#[test]
fn every_sender_gets_the_seen_reaction_including_other_bots() {
    let logs = crate::owner_warning::capture::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (state, gateway, gateway_actions) = make_deps();
            let (event_tx, _event_rx) = create_event_channel();
            let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
            let session_locks = Arc::new(SessionLocks::new());

            let incoming = IncomingMessage::new(
                MessageSource::Discord {
                    guild_id: "111".to_string(),
                    channel_id: "222".to_string(),
                },
                MessageContent::Text("ねえ".to_string()),
                Sender::new("bot-2", "となりのエージェント"),
            )
            .with_metadata(
                "discord_message_id",
                serde_json::Value::String("1234567890123456789".to_string()),
            );

            process_incoming_message(
                incoming,
                gateway,
                state.clone(),
                vec!["crab".to_string()],
                gateway_actions,
                "owner-1".to_string(),
                session_locks,
                false,
                None,
                event_tx,
                registry,
                false,
            )
            .await;
        });
    });

    assert!(
        logs.contains("👀"),
        "他エージェントの投稿に 👀 を付けようとしていない（bot を特別扱いしている）: {logs}"
    );
    // スキップ枝（"Skip reaction: invalid message_id"）も emoji と message_id を
    // ログに出すので、上の assert だけでは「試みた」と「諦めた」を区別できない。
    // 付与を**実際に試みた**（= 送信して 401 で失敗した）ことまで固定する。
    assert!(
        logs.contains("Failed to add reaction"),
        "リアクション付与を試みていない（message_id の解析でスキップされている）: {logs}"
    );
    assert!(
        logs.contains("1234567890123456789"),
        "👀 の付与先メッセージが元の投稿になっていない: {logs}"
    );
}

/// **message_id が無いターンでも落ちない**（付与を諦めるだけ）。
///
/// message_id はメタデータ由来で、欠けることがある。ここで panic すると
/// `spawn_serialized` のタスクごと落ち、セッションの応答経路が壊れる。
#[tokio::test]
async fn no_reply_without_a_message_id_is_skipped_not_fatal() {
    assert!(
        no_reply_reaction_calls("NO_REPLY", "").await.is_empty(),
        "message_id が空なのにリアクションを試みている"
    );
    assert!(
        no_reply_reaction_calls("NO_REPLY", "not-a-number")
            .await
            .is_empty(),
        "数値でない message_id でリアクションを試みている"
    );
}

// ---- #543: デバウンスを (channel, sender) から (channel, 権限レベル) へ移し、run 発火直前で間引く ----

fn discord_msg(channel: &str, sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: channel.to_string(),
        },
        MessageContent::Text(text.to_string()),
        Sender::new(sender, sender),
    )
}

/// #543: record-only パスは記録（と 👀）まで済ませ、推論（run）は起こさない。合流窓の
/// 非トリガーメッセージを「捨てずに記録だけする」ための土台。
#[tokio::test]
async fn record_only_records_inbound_but_does_not_run() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    process_incoming_message(
        discord_msg("222", "user-1", "記録だけして"),
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry,
        true, // record_only
    )
    .await;

    assert_eq!(
        state.inbound_records.lock().unwrap().clone(),
        vec!["記録だけして".to_string()],
        "record_only でも発言は記録される"
    );
    assert!(
        state.runs.lock().unwrap().is_empty(),
        "record_only なのに推論（run）が走った"
    );
}

/// #543 本体: 合流窓が別 sender の複数メッセージを含んでも、**全メッセージが個別に記録され**、
/// **run は 1 回だけ**起きる。run は DB から会話全体を読むので、合流分は全部・正しい帰属で
/// 文脈に入る（＝「ユーザーの発言を捨てる」ことをしない）。
#[tokio::test]
async fn debounced_window_records_every_message_but_runs_once() {
    let (state, gateway, gateway_actions) = make_deps();
    let (event_tx, _event_rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let session_locks = Arc::new(SessionLocks::new());

    // フラッシュの再現: 非トリガーは record-only、内容のある最後のメッセージが run トリガー。
    process_incoming_message(
        discord_msg("222", "kairo", "かいろの発言"),
        gateway.clone(),
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions.clone(),
        "owner-1".to_string(),
        session_locks.clone(),
        false,
        None,
        event_tx.clone(),
        registry.clone(),
        true, // 非トリガー（record-only）
    )
    .await;
    process_incoming_message(
        discord_msg("222", "nostarou", "のすたろうの発言"),
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_locks,
        false,
        None,
        event_tx,
        registry,
        false, // トリガー（run を起こす）
    )
    .await;

    state.wait_for_run().await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert!(
        records.contains(&"かいろの発言".to_string()),
        "records={records:?}"
    );
    assert!(
        records.contains(&"のすたろうの発言".to_string()),
        "records={records:?}"
    );
    assert_eq!(records.len(), 2, "合流しても畳まず 2 件記録: {records:?}");
    assert_eq!(state.runs.lock().unwrap().len(), 1, "合流窓の run は 1 回");
}

/// #556: デバウンス**バッファ**のキーは **channel だけ**（権限では割らない）。同一 channel は
/// 送信者・権限が何であれ同じバッファ、別 channel は別バッファ。権限による run 分割は
/// フラッシュ時のグループ化（`consecutive_groups`）で行うので、キー自体は caller を見ない。
#[test]
fn debounce_window_key_is_channel_only() {
    let a = discord_msg("222", "owner-1", "hi");
    let b = discord_msg("222", "gaikoku", "yo");
    let other_channel = discord_msg("999", "owner-1", "hi");

    assert_eq!(
        debounce_window_key(&a),
        debounce_window_key(&b),
        "同一 channel は送信者（権限）が違っても同じバッファに入るべき"
    );
    assert_ne!(
        debounce_window_key(&a),
        debounce_window_key(&other_channel),
        "別 channel は別バッファのはず"
    );
}

/// #556: フラッシュ時のグループ化 = 到着順のまま「連続した同一 trust_level」で切る。
/// オーナー指示の 4 ケースを固定する（**権限が揃った場合**の挙動。#489 未修正の実運用では
/// co_agent が `Agent`(=0) に落ちるので owner→co_agent は同権限にならない点に注意）。
///
/// run の回数 = 「内容のあるメッセージを含むグループ」の数。4 ケースは全員が内容ありなので
/// run 回数 = グループ数。
#[test]
fn flush_groups_consecutive_same_privilege() {
    use opencrab_actions::CallerIdentity;
    let owner = CallerIdentity::Owner;
    let co_agent = CallerIdentity::CoAgent {
        agent_id: "kairo".to_string(),
    };
    let external = CallerIdentity::Agent;

    // trust_level: Owner=2, CoAgent=2(owner 等価), Agent=0。
    let levels =
        |cs: &[&CallerIdentity]| -> Vec<u8> { cs.iter().map(|c| c.trust_level()).collect() };

    // A(owner) → D(外部) → B(co_agent) = [2,0,2] → [A][D][B] = 3 グループ（run 3 回）。
    assert_eq!(
        consecutive_groups(&levels(&[&owner, &external, &co_agent])),
        vec![(0, 1), (1, 1), (2, 1)],
        "owner→外部→co_agent は権限が 2,0,2 と交互なので 3 グループ"
    );
    // A(owner) → B(co_agent) → D(外部) = [2,2,0] → [A,B][D] = 2 グループ（run 2 回）。
    assert_eq!(
        consecutive_groups(&levels(&[&owner, &co_agent, &external])),
        vec![(0, 2), (2, 1)],
        "owner→co_agent は同権限(2)で合流、外部(0)は別グループ"
    );
    // A(owner) → A(owner) = [2,2] → [A,A] = 1 グループ（run 1 回）。
    assert_eq!(
        consecutive_groups(&levels(&[&owner, &owner])),
        vec![(0, 2)],
        "同一 owner 連続は 1 グループ"
    );
    // D(外部) → D(外部) → A(owner) = [0,0,2] → [D,D][A] = 2 グループ（run 2 回）。
    assert_eq!(
        consecutive_groups(&levels(&[&external, &external, &owner])),
        vec![(0, 2), (2, 1)],
        "外部連続は 1 グループ、owner は別グループ"
    );
}

/// #556 統合: **権限レベルが違うメッセージは同一 channel でも別グループ＝別 run**に分かれる
/// 通しテスト（`run_discord_loop` の flush を一巡）。ここが降格を防ぐ核心: owner の run は
/// caller=Owner のまま走り、外部発言に引きずられない。
///
/// harness の `resolve_caller` は owner-1 を `Owner`(rank 2)・それ以外を `TrustedUser`(rank 1)
/// に解決する。owner→外部は [owner][外部] の 2 グループなので run は 2 回。窓を丸ごと 1 run に
/// する（グループ化を外す）とこのテストが run 1 回で落ちる（回帰ガード）。記録は個別に 2 件。
#[tokio::test]
async fn different_privilege_messages_split_into_separate_runs_through_the_loop() {
    let (state, gateway, gateway_actions) = make_deps();
    let (tx, rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    let loop_state = state.clone();
    let handle = tokio::spawn(super::run_discord_loop(
        gateway,
        loop_state,
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        None,
        Some((tx.clone(), rx)),
        false,
        None,
        registry,
    ));

    // 同一 channel・2 秒窓に Owner（sender=owner-1, rank 2）→ 非 Owner（sender=gaikoku,
    // rank 1=TrustedUser）。連続同権限グループ化なので [owner][外部] の 2 グループ＝2 run。
    for (sender, text) in [
        ("owner-1", "オーナーの発言"),
        ("gaikoku", "外部ユーザーの発言"),
    ] {
        tx.send(super::LoopEvent::IncomingMessage(discord_msg(
            "222", sender, text,
        )))
        .expect("event loop receiver closed");
    }

    state.wait_for_runs(2).await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        2,
        "全メッセージ個別に記録される（畳まない・落とさない）: {records:?}"
    );
    assert!(
        records.contains(&"オーナーの発言".to_string()),
        "{records:?}"
    );
    assert!(
        records.contains(&"外部ユーザーの発言".to_string()),
        "{records:?}"
    );
    assert_eq!(
        state.runs.lock().unwrap().len(),
        2,
        "権限が違うメッセージは別グループ＝別 run（owner と外部で 2 回）"
    );

    // 降格しない: owner の run は caller=Owner、外部の run は caller=TrustedUser。
    // どちらの run にどちらの caller が載るかは順序に依存しうるので集合で確認する。
    let callers = vec![state.observed_caller(0), state.observed_caller(1)];
    assert!(
        callers.contains(&CallerIdentity::Owner),
        "owner の指示は owner 権限の run で走るべき（降格しない）: {callers:?}"
    );
    assert!(
        callers.contains(&CallerIdentity::TrustedUser),
        "外部発言は非 owner 権限の run で走るべき: {callers:?}"
    );

    handle.abort();
}

/// #543: run トリガーは「内容のある最後のメッセージ」。末尾が空でも内容のある方を選び、
/// 全メッセージが空のときだけ run を起こさない（間引きで run を消さない・空を選ばない）。
#[test]
fn run_trigger_selection_uses_latest_message_with_content() {
    let has = discord_msg("222", "u", "本文あり");
    let empty = discord_msg("222", "u", "");
    assert!(incoming_has_content(&has));
    assert!(!incoming_has_content(&empty));

    let window = [has, empty];
    assert_eq!(window.iter().rposition(incoming_has_content), Some(0));

    let all_empty = [discord_msg("222", "u", ""), discord_msg("222", "u", "")];
    assert_eq!(all_empty.iter().rposition(incoming_has_content), None);
}

/// #543/#556 統合: **実イベントループ（`run_discord_loop`）の flush を一巡させる**通しテスト。
///
/// 個別ユニットだけでなく、バッファ蓄積 → デバウンス期限 → フラッシュ → 全メッセージ記録 →
/// run 1 回、という配線そのものを固定する。ここは**全員が同権限（非オーナー = TrustedUser）
/// なので連続同権限グループは 1 つ**＝ run 1 回になる（権限が混ざる場合の分割は
/// `different_privilege_messages_split_into_separate_runs_through_the_loop` が固定する）。
/// **末尾に空メッセージ**を混ぜて「トリガー＝グループ内の内容のある**最後**」（単なる最後では
/// ない）も守る: 空をトリガーに選ぶと run が消え、本テストが落ちる。
#[tokio::test]
async fn debounce_flush_records_all_messages_and_runs_once_through_the_loop() {
    let (state, gateway, gateway_actions) = make_deps();
    let (tx, rx) = create_event_channel();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

    let loop_state = state.clone();
    let handle = tokio::spawn(super::run_discord_loop(
        gateway,
        loop_state,
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        None,
        Some((tx.clone(), rx)),
        false,
        None,
        registry,
    ));

    // 同一チャンネル・同権限（非オーナー = TrustedUser）の内容あり 2 通 ＋ 末尾に空 1 通。
    // 別 sender でも同権限なので 1 グループにまとまり、1 回のフラッシュで 1 run。
    for (sender, text) in [
        ("kairo", "かいろの発言"),
        ("nostarou", "のすたろうの発言"),
        ("nostarou", ""),
    ] {
        tx.send(super::LoopEvent::IncomingMessage(discord_msg(
            "222", sender, text,
        )))
        .expect("event loop receiver closed");
    }

    // 2 秒窓のフラッシュ → トリガー（内容のある最後 = のすたろう）が run を起こすのを待つ。
    state.wait_for_run().await;

    let records = state.inbound_records.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        2,
        "窓の内容ありメッセージは全部個別に記録される（畳まない・落とさない）: {records:?}"
    );
    assert!(records.contains(&"かいろの発言".to_string()), "{records:?}");
    assert!(
        records.contains(&"のすたろうの発言".to_string()),
        "{records:?}"
    );
    assert_eq!(
        state.runs.lock().unwrap().len(),
        1,
        "窓の run は 1 回（末尾の空メッセージでトリガーを落とさない）"
    );

    handle.abort();
}
