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

use std::sync::{Arc, Mutex};

use opencrab_actions::subtask::SubtaskRegistry;
use opencrab_actions::{CallerIdentity, RunRequest, SessionRuntime};
use opencrab_core::EngineResult;
use opencrab_gateway::{DiscordGateway, IncomingMessage, MessageContent, MessageSource, Sender};

use super::{create_event_channel, process_incoming_message, process_subtask_completed};

/// `run_agent_response` の観測 1 件。
///
/// `subtask_registry` を **`Option<SubtaskRegistry>` のまま**保持するのが要点。
/// bool に潰すと同一性の検査ができない。
struct RunObservation {
    session_id: String,
    subtask_registry: Option<SubtaskRegistry>,
    has_completion_sink: bool,
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
    /// run を 1 件観測したことの通知。
    ///
    /// inbound 経路の応答生成は `SessionRuntime::spawn_serialized` の別タスクで走るため、
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
}

#[async_trait::async_trait]
impl opencrab_actions::AgentRuntime for FakeRunner {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.runs.lock().unwrap().push(RunObservation {
            session_id: req.session_id.clone(),
            subtask_registry: req.subtask_registry.clone(),
            has_completion_sink: req.completion_sink.is_some(),
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

    fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {}

    fn record_inbound_message(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _record: &opencrab_actions::InboundMessageRecord<'_>,
    ) {
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
        unimplemented!("この fake は A2UI interaction を使わない")
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
        unimplemented!("この fake は A2UI interaction を使わない")
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
        _sender_id: &str,
        _agent_ids: &[String],
        _owner_discord_id: &str,
    ) -> CallerIdentity {
        CallerIdentity::TrustedUser
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

/// 本番と同じ形の依存一式（ネットワークへは出ない）。
///
/// `DiscordGateway::new` は HTTP クライアントとチャンネルを組むだけで接続しない。
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
    let session_runtime = Arc::new(SessionRuntime::new());

    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text("掘削して".to_string()),
        Sender::user("user-1", "だれか"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_runtime,
        false,
        None,
        event_tx,
        registry.clone(),
    )
    .await;

    // 応答生成は `SessionRuntime::spawn_serialized` の中で走るので、観測の通知を待つ。
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
    let session_runtime = Arc::new(SessionRuntime::new());

    let reply = "[Peer Review] score: 0.7, gaps: none, summary: ok";
    let incoming = IncomingMessage::new(
        MessageSource::Discord {
            guild_id: "111".to_string(),
            channel_id: "222".to_string(),
        },
        MessageContent::Text(reply.to_string()),
        Sender::user("user-1", "crab-b"),
    );

    process_incoming_message(
        incoming,
        gateway,
        state.clone(),
        vec!["crab".to_string()],
        gateway_actions,
        "owner-1".to_string(),
        session_runtime,
        false,
        None,
        event_tx,
        registry,
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
