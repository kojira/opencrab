
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opencrab_actions::subtask::{settle_completed, SettleContext};
use opencrab_actions::{
    AgentRuntime, CallerIdentity, EffortSnapshot, InboundMessageRecord,
    ModelAdminError, ModelAdministration, ModelSnapshot, OutboundReplyRecord, RunRequest,
    SessionEffortList, SessionLocks, SessionModelList, SubtaskLifecycle, SubtaskRegistries,
    TranscriptSource, TurnModelProfile, REASONING_EFFORTS,
};
use opencrab_core::EngineResult;
use opencrab_db::queries::{AgentRow, SessionRow};
use opencrab_extgate::completion::ExtgateCompletionSink;
use opencrab_extgate::{
    invoke_and_wait, now_nanos, recover_stale_calls, recover_stale_deliveries, serve_uds,
    ExtgateOpsGatewayActions, ExtgateState,
};
use opencrab_gate_client::client::{InstanceClient, SaidOutcome};
use opencrab_gateway::{GatewayActions, GatewayCallContext, GatewayCaller};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Notify};
use uuid::Uuid;


#[derive(Clone)]
struct TestRuntime {
    db: opencrab_db::Db,
    locks: Arc<SessionLocks>,
    registries: Arc<SubtaskRegistries>,
    reply: Arc<Mutex<String>>,
    budget_fails: Arc<AtomicBool>,
    turns: Arc<AtomicUsize>,
    hold_rx: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    /// sink 未配線（旧 V3）のときだけ待つ。遅いツールの同期実行相当。
    tool_hold_rx: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    sink_seen: Arc<AtomicBool>,
    conversations: Arc<Mutex<Vec<String>>>,
    system_prompts: Arc<Mutex<Vec<String>>>,
    reply_targets: Arc<Mutex<Vec<Option<String>>>>,
    sender_names: Arc<Mutex<Vec<String>>>,
    images: Arc<Mutex<Vec<Vec<String>>>>,
    turn_entered: Arc<Notify>,
    /// D-1081: every `session_turn_model` profile, in turn-start order.
    turn_profiles: Arc<Mutex<Vec<TurnModelProfile>>>,
    /// D-1085: `RunRequest::lightweight` of every run, in run order.
    lightweight_runs: Arc<Mutex<Vec<bool>>>,
    /// D-1085: `(RunRequest::provisional, promotion_gate.is_some())` of every run, in run order.
    provisional_runs: Arc<Mutex<Vec<(bool, bool)>>>,
    /// D-1085 §12: [`ToolSources`] of every run, in run order.
    tool_sources: Arc<Mutex<Vec<ToolSources>>>,
    last_sink: Arc<Mutex<Option<Arc<dyn opencrab_actions::SubtaskCompletionSink>>>>,
    partial_script: PartialScript,
    /// D-1087: the faked engine's tool/lifecycle steps for the turn.
    activity_script: ActivityScript,
    gated_tool: GatedToolScript,
}

impl TestRuntime {
    fn new(db: opencrab_db::Db) -> Self {
        Self {
            db,
            locks: Arc::new(SessionLocks::new()),
            registries: Arc::new(SubtaskRegistries::new()),
            reply: Arc::new(Mutex::new("hello from agent".into())),
            budget_fails: Arc::new(AtomicBool::new(false)),
            turns: Arc::new(AtomicUsize::new(0)),
            hold_rx: Arc::new(Mutex::new(None)),
            tool_hold_rx: Arc::new(Mutex::new(None)),
            sink_seen: Arc::new(AtomicBool::new(false)),
            conversations: Arc::new(Mutex::new(Vec::new())),
            system_prompts: Arc::new(Mutex::new(Vec::new())),
            reply_targets: Arc::new(Mutex::new(Vec::new())),
            sender_names: Arc::new(Mutex::new(Vec::new())),
            images: Arc::new(Mutex::new(Vec::new())),
            turn_entered: Arc::new(Notify::new()),
            turn_profiles: Arc::new(Mutex::new(Vec::new())),
            lightweight_runs: Arc::new(Mutex::new(Vec::new())),
            provisional_runs: Arc::new(Mutex::new(Vec::new())),
            tool_sources: Arc::new(Mutex::new(Vec::new())),
            last_sink: Arc::new(Mutex::new(None)),
            partial_script: PartialScript::default(),
            activity_script: ActivityScript::default(),
            gated_tool: GatedToolScript::default(),
        }
    }
}

#[async_trait]
impl AgentRuntime for TestRuntime {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.sink_seen
            .store(req.completion_sink.is_some(), Ordering::SeqCst);
        *self.last_sink.lock().unwrap() = req.completion_sink.clone();
        let initial_read_origin = req.initial_read_origin.clone();
        let result_origin = initial_read_origin.clone();
        let on_read_origin = req.on_read_origin.clone();
        self.system_prompts
            .lock()
            .unwrap()
            .push(req.system_prompt.clone());
        self.lightweight_runs.lock().unwrap().push(req.lightweight);
        self.provisional_runs
            .lock()
            .unwrap()
            .push((req.provisional, req.promotion_gate.is_some()));
        self.tool_sources.lock().unwrap().push(tool_sources(&req));
        self.reply_targets
            .lock()
            .unwrap()
            .push(req.reply_target.clone());
        self.conversations.lock().unwrap().push(req.conversation);
        self.images.lock().unwrap().push(req.image_urls.clone());
        // #964: この conformance runtime の Engine 境界を模擬する。request が完成した後、
        // simulated LLM call の直前にだけ initial origin の read 通知を await する。
        if let (Some(origin), Some(cb)) = (initial_read_origin, on_read_origin) {
            cb(origin).await;
        }
        self.partial_script.play(&req.on_partial_text, &req.on_continuation_speech).await;
        self.activity_script
            .play(&req.tool_event_sink, &req.completion_sink);
        self.turn_entered.notify_waiters();
        // 旧 V3（sink 無し）はツールを同期実行する。sink があれば detach 済みなので待たない。
        if req.completion_sink.is_none() {
            let tool_hold = self.tool_hold_rx.lock().unwrap().take();
            if let Some(rx) = tool_hold {
                let _ = rx.await;
            }
        }
        let hold = self.hold_rx.lock().unwrap().take();
        if let Some(rx) = hold {
            let _ = rx.await;
        }
        if let Some(stopped) = self.gated_tool.run(req.promotion_gate.as_ref(), &self.turns) { return stopped; }
        self.turns.fetch_add(1, Ordering::SeqCst);
        let reply = self.reply.lock().unwrap().clone();
        // R3(❌): "__FAIL__" 応答でエンジン失敗を模擬する（DeliveryEffect::Failed 経路）。
        if reply == "__FAIL__" {
            anyhow::bail!("simulated turn failure");
        }
        let folded_origins = if reply == "__VISIBLE_A_SILENT_FOLDED__" {
            // The conformance runtime stands in for the engine; the test admits origin-b while
            // this request is held and returns the engine outcome for that folded inbound.
            vec!["origin-b".to_string()]
        } else {
            Vec::new()
        };
        let reply = if reply == "__VISIBLE_A_SILENT_FOLDED__" {
            "visible-a".to_string()
        } else {
            reply
        };
        let termination = opencrab_core::terminate_at_no_reply(&reply);
        let explicit_termination = termination
            .terminated()
            .then_some(opencrab_core::ExplicitTermination::NoReply);
        let response = if termination.terminated() {
            termination.speech().unwrap_or_default().to_string()
        } else {
            reply
        };
        let silent_origins = if !folded_origins.is_empty() {
            folded_origins
        } else if response.trim().is_empty() && explicit_termination.is_some() {
            result_origin.into_iter().collect()
        } else {
            Vec::new()
        };
        Ok(EngineResult {
            response,
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit: false,
            explicit_termination,
            silent_origins,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        })
    }
    fn build_agent_context(&self, agent_id: &str, _caller: &CallerIdentity) -> (String, String) {
        ("sys".into(), agent_id.to_string())
    }
    fn build_conversation_string(
        &self,
        session_id: &str,
        _agent_id: &str,
        _model: &str,
        _budget: usize,
        _system_prompt: &str,
        _runtime_context_text: &str,
        window: Option<opencrab_core::conversation::ConversationWindow<'_>>,
    ) -> anyhow::Result<String> {
        let conn = self.db.lock().unwrap();
        let rows = opencrab_db::queries::list_session_logs_by_session(&conn, session_id)?;
        let rows = match window.map(|window| window.rows) {
            Some(opencrab_core::conversation::RecentWindow::Snapshot { high_water, pending }) => {
                rows.into_iter()
                    .filter(|r| r.id.is_some_and(|id| id <= high_water))
                    .chain(pending.iter().cloned())
                    .collect()
            }
            _ => rows,
        };
        Ok(rows
            .into_iter()
            .map(|r| format!("{}:{}", r.log_type, r.content))
            .collect::<Vec<_>>()
            .join("\n"))
    }
    fn context_budget_tokens(
        &self,
        _agent_id: &str,
        _session_id: &str,
        _model: &str,
        _system_prompt: &str,
        _runtime_context_text: &str,
    ) -> std::result::Result<usize, opencrab_core::context_budget::ContextBudgetError> {
        if self.budget_fails.load(Ordering::SeqCst) {
            return Err(
                opencrab_core::context_budget::ContextBudgetError::MissingContextWindow(
                    "simulated budget failure".into(),
                ),
            );
        }
        Ok(1024)
    }
    fn session_turn_model(
        &self,
        _agent_id: &str,
        _session_id: &str,
        profile: TurnModelProfile,
    ) -> anyhow::Result<String> {
        self.turn_profiles.lock().unwrap().push(profile);
        Ok("mock:test".to_string())
    }
    fn has_llm_providers(&self) -> bool {
        true
    }
    fn agent_exists(&self, _agent_id: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
    fn session_locks(&self) -> Arc<SessionLocks> {
        self.locks.clone()
    }
    fn subtask_registry_for(&self, session_id: &str) -> opencrab_actions::SubtaskRegistry {
        self.registries.registry_for(session_id)
    }
    fn record_inbound_message(
        &self,
        _source: TranscriptSource,
        _record: &InboundMessageRecord<'_>,
    ) -> bool {
        true
    }
    fn on_inbound_message(
        &self,
        _source: TranscriptSource,
        _agent_id: &str,
        record: &InboundMessageRecord<'_>,
    ) {
        self.sender_names
            .lock()
            .unwrap()
            .push(record.sender_name.to_string());
    }
    fn record_outbound_reply(&self, _source: TranscriptSource, _record: &OutboundReplyRecord<'_>) {}
    fn ensure_session(
        &self,
        _session_id: &str,
        _agent_ids: &[String],
        _theme: &str,
        _metadata_json: &str,
        _mode: &str,
    ) {
    }
    fn session_theme(&self, _session_id: &str) -> Option<String> {
        None
    }
}

struct Harness {
    state: Arc<ExtgateState>,
    runtime: TestRuntime,
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
    subject_id: i64,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = opencrab_db::Db::memory().unwrap();
        let subject_id = {
            let mut conn = db.lock().unwrap();
            recover_stale_deliveries(&mut conn, now_nanos()).unwrap();
            opencrab_db::queries::upsert_agent(
                &conn,
                &AgentRow {
                    agent_id: "agent-1".into(),
                    name: "A".into(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: "p".into(),
                    personality: None,
                    instructions: String::new(),
                    model: None,
                    reasoning_effort: None,
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
            conn.query_row(
                "SELECT subject_id FROM agents WHERE agent_id='agent-1'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        Self::start_with_db(dir, db, subject_id).await
    }

    async fn start_with_db(
        dir: tempfile::TempDir,
        db: opencrab_db::Db,
        subject_id: i64,
    ) -> Self {
        let sock = dir.path().join("gate.sock");
        let state = Arc::new(ExtgateState::new(db.clone()));
        let runtime = TestRuntime::new(db);
        let listen_state = Arc::clone(&state);
        let rt = runtime.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(listen_state, rt, path).await;
        });
        for _ in 0..200 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            state,
            runtime,
            sock,
            _dir: dir,
            subject_id,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.sock).await.expect("connect")
    }

}

fn uuid() -> String {
    Uuid::new_v4().to_string()
}

async fn write_frame(s: &mut UnixStream, v: &Value) {
    let mut buf = serde_json::to_vec(v).unwrap();
    buf.push(b'\n');
    s.write_all(&buf).await.unwrap();
}

async fn read_frame(s: &mut UnixStream) -> Value {
    let mut buf = Vec::new();
    loop {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.expect("read");
        if b[0] == b'\n' {
            break;
        }
        buf.push(b[0]);
    }
    serde_json::from_slice(&buf).unwrap()
}

async fn read_until(s: &mut UnixStream, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..40 {
        if let Some(v) = read_frame_opt(s).await {
            if pred(&v) {
                return v;
            }
        }
    }
    panic!("expected frame not received");
}

async fn read_said_response(s: &mut UnixStream, id: &str) -> Value {
    read_until(s, |v| v["id"] == id).await
}

async fn read_frame_opt(s: &mut UnixStream) -> Option<Value> {
    let read = async {
        let mut buf = Vec::new();
        loop {
            let mut b = [0u8; 1];
            s.read_exact(&mut b).await.ok()?;
            if b[0] == b'\n' {
                break;
            }
            buf.push(b[0]);
        }
        serde_json::from_slice(&buf).ok()
    };
    tokio::time::timeout(Duration::from_millis(250), read)
        .await
        .ok()
        .flatten()
}

/// D-1099: an instance exists once a gateway says `hello`; tests may also create the row up front
/// (with any `kind_id`) to model an instance that predates the change.
async fn put_instance(h: &Harness, instance_id: &str, enabled: bool) -> Value {
    put_instance_kind(h, instance_id, enabled, "gateway").await
}

async fn put_instance_kind(h: &Harness, instance_id: &str, enabled: bool, kind_id: &str) -> Value {
    let conn = h.state.db.lock().unwrap();
    let now = now_nanos();
    conn.execute(
        "INSERT OR IGNORE INTO gate_instances (
            instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest,
            created_at, updated_at, deleted_at
         ) VALUES (?1, ?2, ?3, 1, ?4, 'e30=', ?5, ?6, ?6, NULL)",
        rusqlite::params![
            instance_id,
            kind_id,
            h.subject_id,
            i64::from(enabled),
            opencrab_extgate::ids::config_digest_from_b64("e30=").unwrap(),
            now
        ],
    )
    .unwrap();
    json!({"instance_id": instance_id})
}

/// Core's internal row id for `(instance, key)`, resolving (creating) it like a said does.
fn session_row(h: &Harness, instance_id: &str, key: &str) -> opencrab_db::queries::GateSession {
    let mut conn = h.state.db.lock().unwrap();
    let tx = conn.transaction().unwrap();
    let found =
        opencrab_db::queries::resolve_gate_session_in_tx(&tx, instance_id, key, None).unwrap();
    tx.commit().unwrap();
    found
}

/// A pre-D-1099 record: row `row_id` for `(instance, key)` with session `session_id` (title
/// `title`). Models a migrated binding whose id and session predate the session-key wire.
fn legacy_record(h: &Harness, row_id: &str, instance_id: &str, key: &str, session_id: &str, title: &str) {
    let conn = h.state.db.lock().unwrap();
    if opencrab_db::queries::get_session(&conn, session_id).unwrap().is_none() {
        conn.execute(
            "INSERT INTO sessions (id, theme, created_at, updated_at) VALUES (?1, ?2, 'x', 'x')",
            rusqlite::params![session_id, title],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES ('agent-1', ?1)",
            [session_id],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        rusqlite::params![row_id, instance_id, key, now_nanos(), session_id],
    )
    .unwrap();
}

async fn hello_ok(s: &mut UnixStream, instance_id: &str) {
    write_frame(
        s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "automatic",
            "operations": [],
            "instance_id": instance_id,
            "agent_id": "agent-1",
        }),
    )
    .await;
    let ok = read_frame(s).await;
    assert_eq!(ok["m"], "ok");
    assert_eq!(ok["id"], "h1");
}

/// A connected instance and the session key its frames use. The third value is the key (it plays
/// the role the old binding id did in the test bodies).
async fn ready_pair(h: &Harness) -> (UnixStream, String, String) {
    let instance_id = uuid();
    let key = "chan-1".to_string();
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id).await;
    (s, instance_id, key)
}

/// Adds a second agent (its own subject).
fn add_agent(h: &Harness, agent_id: &str) {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: agent_id.into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
}
