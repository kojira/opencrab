
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use opencrab_actions::subtask::{settle_completed, SettleContext};
use opencrab_actions::{
    AgentRuntime, CallerIdentity, InboundMessageRecord, InteractionRecord, OutboundReplyRecord,
    RunRequest, SessionLocks, SubtaskLifecycle, SubtaskRegistries, TranscriptSource,
};
use opencrab_core::EngineResult;
use opencrab_db::queries::{AgentRow, SessionRow, TRUSTED_PLATFORM_EXTGATE};
use opencrab_extgate::completion::ExtgateCompletionSink;
use opencrab_extgate::{
    admin_router, invoke_and_wait, now_nanos, recover_stale_calls, recover_stale_deliveries,
    resolve_caller_identity_with_owner, serve_uds, session_id_for_binding, validate_listen_socket,
    DeliveryMode, ExtgateOpsGatewayActions, ExtgateState, NostrBundleAdmit, NostrSaidDecision,
    NostrWatchSets, OperatorToken, UNAUTHORIZED_BODY,
};
use opencrab_gate_client::client::{InstanceClient, SaidOutcome};
use opencrab_gateway::{GatewayActions, GatewayCallContext};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Notify};
use tower::ServiceExt;
use uuid::Uuid;

const TOKEN: &str = "operator-token";

#[derive(Clone)]
struct TestRuntime {
    db: opencrab_db::Db,
    locks: Arc<SessionLocks>,
    registries: Arc<SubtaskRegistries>,
    reply: Arc<Mutex<String>>,
    turns: Arc<AtomicUsize>,
    hold_rx: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    /// sink 未配線（旧 V3）のときだけ待つ。遅いツールの同期実行相当。
    tool_hold_rx: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    sink_seen: Arc<AtomicBool>,
    conversations: Arc<Mutex<Vec<String>>>,
    sender_names: Arc<Mutex<Vec<String>>>,
    images: Arc<Mutex<Vec<Vec<String>>>>,
    turn_entered: Arc<Notify>,
}

impl TestRuntime {
    fn new(db: opencrab_db::Db) -> Self {
        Self {
            db,
            locks: Arc::new(SessionLocks::new()),
            registries: Arc::new(SubtaskRegistries::new()),
            reply: Arc::new(Mutex::new("hello from agent".into())),
            turns: Arc::new(AtomicUsize::new(0)),
            hold_rx: Arc::new(Mutex::new(None)),
            tool_hold_rx: Arc::new(Mutex::new(None)),
            sink_seen: Arc::new(AtomicBool::new(false)),
            conversations: Arc::new(Mutex::new(Vec::new())),
            sender_names: Arc::new(Mutex::new(Vec::new())),
            images: Arc::new(Mutex::new(Vec::new())),
            turn_entered: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl AgentRuntime for TestRuntime {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.sink_seen
            .store(req.completion_sink.is_some(), Ordering::SeqCst);
        let initial_read_origin = req.initial_read_origin.clone();
        let on_read_origin = req.on_read_origin.clone();
        self.conversations.lock().unwrap().push(req.conversation);
        self.images.lock().unwrap().push(req.image_urls.clone());
        // #964: この conformance runtime の Engine 境界を模擬する。request が完成した後、
        // simulated LLM call の直前にだけ initial origin の read 通知を await する。
        if let (Some(origin), Some(cb)) = (initial_read_origin, on_read_origin) {
            cb(origin).await;
        }
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
        self.turns.fetch_add(1, Ordering::SeqCst);
        let reply = self.reply.lock().unwrap().clone();
        // R3(❌): "__FAIL__" 応答でエンジン失敗を模擬する（DeliveryEffect::Failed 経路）。
        if reply == "__FAIL__" {
            anyhow::bail!("simulated turn failure");
        }
        Ok(EngineResult {
            response: reply,
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit: false,
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
        _budget: usize,
        _system_prompt: &str,
        _runtime_context_text: &str,
    ) -> anyhow::Result<String> {
        let conn = self.db.lock().unwrap();
        let rows = opencrab_db::queries::list_session_logs_by_session(&conn, session_id)?;
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
        _system_prompt: &str,
        _runtime_context_text: &str,
    ) -> std::result::Result<usize, opencrab_core::context_budget::ContextBudgetError> {
        Ok(1024)
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
    fn record_interaction_response(
        &self,
        _agent_id: &str,
        _session_id: &str,
        _record: &InteractionRecord<'_>,
    ) {
    }
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
    fn mark_interaction_status(
        &self,
        _interaction_id: &str,
        _status: &str,
        _response_json: Option<&str>,
        _responder_id: Option<&str>,
    ) {
    }
    fn cleanup_stale_interactions(&self) {}
    fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {}
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
        let sock = dir.path().join("gate.sock");
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
                    heartbeat_instructions: String::new(),
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
        let state = Arc::new(ExtgateState::new(
            db.clone(),
            OperatorToken::from_bytes(TOKEN),
        ));
        let runtime = TestRuntime::new(db);
        let listen_state = Arc::clone(&state);
        let rt = runtime.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(listen_state, rt, resolve_caller_identity_with_owner, path).await;
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

    async fn admin(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let app = admin_router(Arc::clone(&self.state));
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body)
    }
}

fn uuid() -> String {
    Uuid::new_v4().to_string()
}

fn config_b64() -> &'static str {
    "e30="
}

fn config_digest() -> String {
    opencrab_extgate::ids::config_digest_from_b64(config_b64()).unwrap()
}

fn auth() -> String {
    format!("Bearer {TOKEN}")
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

async fn put_instance(h: &Harness, instance_id: &str, enabled: bool) -> Value {
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "discord",
                        "subject_id": h.subject_id,
                        "enabled": enabled,
                        "config_b64": config_b64(),
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

async fn put_binding(
    h: &Harness,
    binding_id: &str,
    instance_id: &str,
    address: &str,
) -> StatusCode {
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"instance_id": instance_id, "address": address}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    st
}

async fn hello_ok(s: &mut UnixStream, instance_id: &str, revision: u64) {
    write_frame(
        s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": revision,
            "config_digest": config_digest(),
        }),
    )
    .await;
    let ok = read_frame(s).await;
    assert_eq!(ok["m"], "ok");
    assert_eq!(ok["id"], "h1");
}

async fn ack_bind(s: &mut UnixStream) -> String {
    let bind = read_frame(s).await;
    assert_eq!(bind["m"], "bind");
    let id = bind["id"].as_str().unwrap().to_string();
    let binding_id = bind["binding_id"].as_str().unwrap().to_string();
    write_frame(s, &json!({"id": id, "m": "ok"})).await;
    binding_id
}

async fn ready_pair(h: &Harness) -> (UnixStream, String, String) {
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(h, &instance_id, true).await;
    put_binding(h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let acked = ack_bind(&mut s).await;
    assert_eq!(acked, binding_id);
    for _ in 0..50 {
        let acked = h
            .state
            .lock_registry()
            .unwrap()
            .get(&instance_id)
            .is_some_and(|e| e.acknowledged.contains(&binding_id));
        if acked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (s, instance_id, binding_id)
}

fn err_code(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["error"]["code"].as_str().unwrap().to_string()
}

fn insert_named_session(h: &Harness, id: &str) {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &SessionRow {
            id: id.into(),
            mode: "solo".into(),
            theme: id.into(),
            phase: "convergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: r#"["agent-1"]"#.into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

async fn wait_client_bound(client: &InstanceClient, address: &str, binding_id: &str) {
    for _ in 0..80 {
        if client.binding_for_address(address).await.as_deref() == Some(binding_id) {
            return;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
    }
    panic!("bind ack");
}
/// live entry が消える（切断が反映される）まで待つ。
async fn wait_not_live(h: &Harness, instance_id: &str) {
    for _ in 0..200 {
        if h.state.lock_registry().unwrap().get(instance_id).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("live entry が消えない: {instance_id}");
}

