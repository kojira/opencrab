
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use opencrab_actions::subtask::{settle_completed, SettleContext};
use opencrab_actions::{
    AgentRuntime, CallerIdentity, InboundMessageRecord, InteractionRecord, ModelAdminError,
    ModelAdministration, ModelSnapshot, OutboundReplyRecord, RunRequest, SessionLocks,
    SubtaskLifecycle, SubtaskRegistries, TranscriptSource,
};
use opencrab_core::EngineResult;
use opencrab_db::queries::{AgentRow, SessionRow, TRUSTED_PLATFORM_EXTGATE};
use opencrab_extgate::completion::ExtgateCompletionSink;
use opencrab_extgate::{
    admin_router, invoke_and_wait, now_nanos, recover_stale_calls, recover_stale_deliveries,
    serve_uds, session_id_for_binding, validate_listen_socket, DeliveryMode,
    ExtgateOpsGatewayActions, ExtgateState, UNAUTHORIZED_BODY,
};
use opencrab_gate_client::client::{InstanceClient, SaidOutcome};
use opencrab_gateway::{GatewayActions, GatewayCallContext};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Notify};
use tower::ServiceExt;
use uuid::Uuid;

const TOKEN: &str = "database-backed-fixture";
const FIXTURE_AUTHORIZATION: &str = "Bearer database-backed-fixture";
static NEXT_ADMIN_PRINCIPAL: AtomicUsize = AtomicUsize::new(1);

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
        }
    }
}

#[async_trait]
impl ModelAdministration for TestRuntime {
    async fn list_models(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        let configured_model = self
            .db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .query_row(
                "SELECT model FROM agents WHERE agent_id = ?1",
                [agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|_| ModelAdminError::Internal)?;
        Ok(ModelSnapshot {
            models: vec!["openai:gpt-5".to_string()],
            current_model: configured_model
                .clone()
                .unwrap_or_else(|| "mock:test".to_string()),
            configured_model,
            default_model: "mock:test".to_string(),
        })
    }

    async fn set_model(
        &self,
        agent_id: &str,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        if model != "openai:gpt-5" && model != "gpt-5" {
            return Err(ModelAdminError::NotFound);
        }
        self.db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .execute(
                "UPDATE agents SET model = 'openai:gpt-5' WHERE agent_id = ?1",
                [agent_id],
            )
            .map_err(|_| ModelAdminError::Internal)?;
        self.list_models(agent_id).await
    }

    async fn reset_model(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        self.db
            .lock()
            .map_err(|_| ModelAdminError::Internal)?
            .execute("UPDATE agents SET model = NULL WHERE agent_id = ?1", [agent_id])
            .map_err(|_| ModelAdminError::Internal)?;
        self.list_models(agent_id).await
    }
}

#[async_trait]
impl AgentRuntime for TestRuntime {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.sink_seen
            .store(req.completion_sink.is_some(), Ordering::SeqCst);
        let initial_read_origin = req.initial_read_origin.clone();
        let result_origin = initial_read_origin.clone();
        let on_read_origin = req.on_read_origin.clone();
        self.system_prompts
            .lock()
            .unwrap()
            .push(req.system_prompt.clone());
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
        if self.budget_fails.load(Ordering::SeqCst) {
            return Err(
                opencrab_core::context_budget::ContextBudgetError::MissingContextWindow(
                    "simulated budget failure".into(),
                ),
            );
        }
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
        Self::start_with_db(dir, db, subject_id).await
    }

    async fn start_with_db(
        dir: tempfile::TempDir,
        db: opencrab_db::Db,
        subject_id: i64,
    ) -> Self {
        let sock = dir.path().join("gate.sock");
        let state = Arc::new(ExtgateState::new_protected(db.clone()));
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

    async fn admin(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let req = self.with_database_backed_admin(req).await;
        let app = admin_router(Arc::clone(&self.state));
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body)
    }

    async fn with_database_backed_admin(&self, req: Request<Body>) -> Request<Body> {
        if req.headers().get(header::AUTHORIZATION).and_then(|value| value.to_str().ok())
            != Some(FIXTURE_AUTHORIZATION)
        {
            return req;
        }
        let (mut parts, body) = req.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let path = parts.uri.path();
        let operation = match (parts.method.as_str(), path.contains("/gate-bindings/"), path.ends_with("/revisions")) {
            ("GET", false, false) => "instance.read",
            ("PUT", false, false) => "instance.put",
            ("DELETE", false, false) => "instance.delete",
            ("POST", false, true) => "instance.revise",
            ("PUT", true, false) => "binding.put",
            ("DELETE", true, false) => "binding.delete",
            _ => return Request::from_parts(parts, Body::from(body)),
        };
        let route_id = path.rsplit('/').nth(usize::from(path.ends_with("/revisions"))).unwrap();
        let instance_id = if path.contains("/gate-instances/") {
            route_id.to_owned()
        } else if let Some(instance_id) = body_json.get("instance_id").and_then(Value::as_str) {
            instance_id.to_owned()
        } else {
            let conn = self.state.db.lock().unwrap();
            conn.query_row(
                "SELECT instance_id FROM gate_bindings WHERE binding_id=?1",
                [route_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| Uuid::nil().to_string())
        };

        let sequence = NEXT_ADMIN_PRINCIPAL.fetch_add(1, Ordering::SeqCst) as u64;
        let mut token = [0_u8; 32];
        token[..8].copy_from_slice(&sequence.to_be_bytes());
        token[8..].fill(0x5a);
        let salt = [0x3c_u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(b"opencrab/gate-admin/bearer/v1\0");
        hasher.update(salt);
        hasher.update(token);
        let hash = hasher.finalize().to_vec();
        let principal_id = format!("conformance-{sequence}");
        {
            let conn = self.state.db.lock().unwrap();
            conn.execute(
                "INSERT INTO gate_admin_principals
                 (principal_id, credential_salt, credential_hash, scope_mode, created_at,
                  expires_at, revoked_at, sealed_at, predecessor_principal_id, overlap_deadline)
                 VALUES (?1, ?2, ?3, 'exact', 1, 4000000000000000000, NULL, NULL, NULL, NULL)",
                rusqlite::params![principal_id, salt.as_slice(), hash],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gate_admin_principal_operations VALUES (?1, ?2)",
                rusqlite::params![principal_id, operation],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gate_admin_principal_subjects VALUES (?1, ?2)",
                rusqlite::params![principal_id, self.subject_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gate_admin_principal_instances VALUES (?1, ?2)",
                rusqlite::params![principal_id, instance_id],
            )
            .unwrap();
            conn.execute(
                "UPDATE gate_admin_principals SET sealed_at=2 WHERE principal_id=?1",
                [principal_id],
            )
            .unwrap();
        }
        parts.headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", URL_SAFE_NO_PAD.encode(token)).parse().unwrap(),
        );
        Request::from_parts(parts, Body::from(body))
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
    let grant = {
        let mut conn = h.state.db.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM gate_instances WHERE instance_id=?1",
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        if exists == 0 {
            Some(
                opencrab_db::queries::issue_subject_association_grant(
                    &mut conn,
                    "agent-1",
                    h.subject_id,
                    i64::MAX,
                    now_nanos(),
                )
                .unwrap(),
            )
        } else {
            None
        }
    };
    let mut request = json!({
        "kind_id": "discord",
        "subject_id": h.subject_id,
        "enabled": enabled,
        "config_b64": config_b64(),
    });
    if let Some(grant) = grant {
        request["subject_grant"] = json!(grant);
    }
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(request.to_string()))
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
    let (session_id, title) = {
        let conn = h.state.db.lock().unwrap();
        match opencrab_db::queries::get_session(&conn, address).unwrap() {
            Some(session) => (address.to_string(), session.theme),
            None => (session_id_for_binding(binding_id), address.to_string()),
        }
    };
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "instance_id": instance_id,
                        "address": address,
                        "session": {"session_id": session_id, "title": title}
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

