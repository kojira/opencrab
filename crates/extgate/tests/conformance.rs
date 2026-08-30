//! V3 §9 conformance。mock gateway が omoikane 役。

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
    admin_router, invoke_and_wait, now_nanos, recover_stale_deliveries,
    resolve_caller_identity_with_owner, serve_uds, session_id_for_binding, validate_listen_socket,
    DeliveryMode, ExtgateState, NostrBundleAdmit, NostrSaidDecision, NostrWatchSets, OperatorToken,
    UNAUTHORIZED_BODY,
};
use opencrab_gate_client::client::{InstanceClient, SaidOutcome};
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
            turn_entered: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl AgentRuntime for TestRuntime {
    async fn run_agent_response(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
        self.sink_seen
            .store(req.completion_sink.is_some(), Ordering::SeqCst);
        self.conversations.lock().unwrap().push(req.conversation);
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
        Ok(EngineResult {
            response: self.reply.lock().unwrap().clone(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit: false,
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
    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str) {
        let conn = self.db.lock().unwrap();
        let _ = opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: "NO_REPLY".to_string(),
                speaker_id: Some(agent_id.to_string()),
                turn_number: None,
                metadata_json: Some(r#"{"no_reply":true}"#.into()),
                created_at: None,
            },
        );
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
        _record: &InboundMessageRecord<'_>,
    ) {
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

#[tokio::test]
async fn framing_max_size_ok_and_too_large_closes() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    let mut ok = vec![b'{'; 1_048_575];
    ok[0] = b'{';
    ok[1] = b'"';
    ok[2] = b'm';
    ok[3] = b'"';
    ok[4] = b':';
    ok[5] = b'"';
    // 1,048,576 including LF: send a valid small said instead for success path
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": instance_id, // wrong on purpose? use real binding below
            "origin": "o",
            "author_id": "u1",
            "text": "x",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame_opt(&mut s).await;

    let mut s2 = h.connect().await;
    let mut huge = vec![b'x'; 1_048_577];
    huge[1_048_576] = b'\n';
    s2.write_all(&huge).await.unwrap();
    let mut leftover = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s2.read_to_end(&mut leftover))
        .await
        .expect("too_large did not close")
        .expect("read after too_large");
    assert!(
        leftover.is_empty(),
        "id 未抽出の too_large は err frame 0: {leftover:?}"
    );
}

#[tokio::test]
async fn framing_invalid_utf8_json_non_object_and_duplicates_close() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    s.write_all(b"\x80\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(b"not-json\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(b"[1]\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(br#"{"m":"hello","m":"hello"}"#).await.unwrap();
    s.write_all(b"\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }
}

#[tokio::test]
async fn hello_unknown_fields_ignored_and_missing_fields_fail() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest(),
            "extra": true
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["m"], "ok");

    let mut s = h.connect().await;
    write_frame(&mut s, &json!({"id":"h2","m":"hello","protocol":2})).await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "bad_request");
}

#[tokio::test]
async fn protocol_order_before_hello_and_second_hello() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": uuid(),
            "origin": "o",
            "author_id": "u",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_order");

    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "h2",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_order");
}

#[tokio::test]
async fn response_invalid_unknown_and_consumed_ids() {
    let h = Harness::start().await;
    let (mut s, _, _) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id":"nope","m":"ok"})).await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "response_invalid");
}

#[tokio::test]
async fn hello_failures_do_not_register() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 1,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_unsupported");
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": uuid(),
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "instance_unknown");

    let disabled = uuid();
    put_instance(&h, &disabled, false).await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": disabled,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "instance_disabled");

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 9,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "revision_mismatch");

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "config_digest_mismatch");
}

#[tokio::test]
async fn double_live_hello_is_instance_active() {
    let h = Harness::start().await;
    let (s1, instance_id, _) = ready_pair(&h).await;
    let mut s2 = h.connect().await;
    write_frame(
        &mut s2,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s2).await.unwrap();
    assert_eq!(v["code"], "instance_active");
    drop(s1);
}

#[tokio::test]
async fn registry_starts_empty() {
    let h = Harness::start().await;
    assert!(!h.state.lock_registry().unwrap().is_live(&uuid()));
}

#[tokio::test]
async fn said_before_ack_is_instance_not_ready() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["m"], "bind");
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["m"], "err");
    assert_eq!(v["code"], "instance_not_ready");
}

#[tokio::test]
async fn dynamic_binding_put_keeps_old_said_and_new_not_ready() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_a) = ready_pair(&h).await;
    let binding_b = uuid();
    put_binding(&h, &binding_b, &instance_id, "chan-2").await;
    let _ = read_frame_opt(&mut s).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_a,
            "origin": "oa",
            "author_id": "u1",
            "text": "old",
            "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s1").await;
    assert_eq!(ok["m"], "ok");
    assert!(ok["seq"].as_i64().unwrap() >= 1);
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_b,
            "origin": "ob",
            "author_id": "u1",
            "text": "new",
            "attachments": []
        }),
    )
    .await;
    let err = read_said_response(&mut s, "s2").await;
    assert_eq!(err["code"], "instance_not_ready");
}

#[tokio::test]
async fn live_revision_and_delete_are_409() {
    let h = Harness::start().await;
    let (_s, instance_id, _) = ready_pair(&h).await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("POST")
                .uri(format!("/api/gate-instances/{instance_id}/revisions"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": 1,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_active");

    let (st, body) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_active");
}

#[tokio::test]
async fn live_binding_delete_stops_said() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o",
            "author_id": "u",
            "text": "x",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["code"], "binding_closed");
}

#[tokio::test]
async fn instance_put_idempotent_and_conflict() {
    let h = Harness::start().await;
    let id = uuid();
    put_instance(&h, &id, true).await;
    let st = {
        let (st, _) = h
            .admin(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/gate-instances/{id}"))
                    .header(header::AUTHORIZATION, auth())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "kind_id": "discord",
                            "subject_id": h.subject_id,
                            "enabled": true,
                            "config_b64": config_b64()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        st
    };
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "other",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");
}

#[tokio::test]
async fn bearer_exact_401_and_env_scrub() {
    std::env::set_var("OPENCRAB_GATE_OPERATOR_TOKEN", "env-secret");
    let token = OperatorToken::take_from_env();
    assert!(std::env::var("OPENCRAB_GATE_OPERATOR_TOKEN").is_err());
    assert!(format!("{token:?}").contains("redacted"));
    assert!(!format!("{token:?}").contains("env-secret"));

    let h = Harness::start().await;
    let id = uuid();
    let cases: Vec<Request<Body>> = vec![
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Basic x")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Bearer short")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}x"))
            .body(Body::empty())
            .unwrap(),
    ];
    for req in cases {
        let (st, body) = h.admin(req).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(body, UNAUTHORIZED_BODY);
    }
}

#[tokio::test]
async fn bearer_equal_reaches_operation() {
    let h = Harness::start().await;
    let id = uuid();
    let (st, body) = h
        .admin(
            Request::builder()
                .method("GET")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(err_code(&body), "instance_unknown");
}

#[tokio::test]
async fn said_dedup_same_origin_and_separate_bindings() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_a) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_a,
            "origin": "same",
            "author_id": "u1",
            "text": "one",
            "attachments": []
        }),
    )
    .await;
    let first = read_said_response(&mut s, "s1").await;
    assert_eq!(first["seq"], 1);
    let turns = h.runtime.turns.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_a,
            "origin": "same",
            "author_id": "u1",
            "text": "two",
            "attachments": []
        }),
    )
    .await;
    let again = read_said_response(&mut s, "s2").await;
    assert_eq!(again["seq"], 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), turns);

    let binding_b = uuid();
    put_binding(&h, &binding_b, &instance_id, "chan-b").await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["binding_id"], binding_b);
    write_frame(&mut s, &json!({"id": bind["id"], "m": "ok"})).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s3",
            "m": "said",
            "binding_id": binding_b,
            "origin": "same",
            "author_id": "u1",
            "text": "other",
            "attachments": []
        }),
    )
    .await;
    let other = read_said_response(&mut s, "s3").await;
    assert_eq!(other["seq"], 1);
}

#[tokio::test]
async fn said_seq_null_when_not_recorded_and_lookups_are_real() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.state.probe.whitelist_override.lock().unwrap() = Some(false);
    let accepts = h.state.probe.accept_inbound_count.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "drop",
            "author_id": "u1",
            "text": "no",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["m"], "ok");
    assert!(v["seq"].is_null());
    assert!(h.state.probe.accept_inbound_count.load(Ordering::SeqCst) > accepts);
    assert!(h.state.probe.lookup_wl_count.load(Ordering::SeqCst) > 0);
    *h.state.probe.whitelist_override.lock().unwrap() = None;
}

#[tokio::test]
async fn empty_said_is_bad_request_no_record() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "e",
            "author_id": "u",
            "text": "",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "bad_request");
}

#[tokio::test]
async fn image_only_said_is_recorded_and_starts_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let before = h.runtime.turns.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "img",
            "author_id": "u1",
            "text": "",
            "attachments": [{"kind":"image","url":"https://example.com/a.png"}]
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["seq"], 1);
    for _ in 0..50 {
        if h.runtime.turns.load(Ordering::SeqCst) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.runtime.turns.load(Ordering::SeqCst) > before);
    assert_eq!(
        h.state
            .probe
            .start_session_turn_count
            .load(Ordering::SeqCst),
        h.runtime.turns.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn delivery_ok_rejected_and_disconnect() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "d1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut saw_say = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                saw_say = Some(v);
                break;
            }
        }
    }
    let say = saw_say.expect("say");
    // 単一メンション turn の say は発端 said の origin を reply_target に載せる（gateway が
    // e-tag reply する。gateway の pending_turn 相関は activity ended が say より先に届き
    // 消えるため、返信先は payload で明示する）。
    assert_eq!(
        say["payload"],
        json!({"text": "hello from agent", "reply_target": "d1"})
    );
    assert!(!say["payload"]["text"].as_str().unwrap().is_empty());
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn = h.state.db.lock().unwrap();
    let state: String = conn
        .query_row(
            "SELECT state FROM deliveries WHERE delivery_id = ?1",
            [say["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "delivered");
    drop(conn);

    *h.runtime.reply.lock().unwrap() = "second".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_id,
            "origin": "d2",
            "author_id": "u1",
            "text": "again",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut say2 = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                say2 = Some(v);
                break;
            }
        }
    }
    let say2 = say2.expect("say2");
    write_frame(
        &mut s,
        &json!({
            "id": say2["id"],
            "m": "err",
            "code": "external_rejected",
            "detail": null
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn = h.state.db.lock().unwrap();
    let (st, err): (String, String) = conn
        .query_row(
            "SELECT state, error FROM deliveries WHERE delivery_id = ?1",
            [say2["id"].as_str().unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(st, "failed");
    assert_eq!(err, "external_rejected");
}

#[tokio::test]
async fn delivery_disconnect_is_indeterminate_and_no_resend() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "cut",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut delivery_id = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                delivery_id = Some(v["id"].as_str().unwrap().to_string());
                break;
            }
        }
    }
    let delivery_id = delivery_id.expect("say");
    drop(s);
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let (st, err): (String, String) = conn
        .query_row(
            "SELECT state, error FROM deliveries WHERE delivery_id = ?1",
            [&delivery_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(st, "indeterminate");
    assert_eq!(err, "disconnect");
    drop(conn);

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let _ = ack_bind(&mut s).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Some(v) = read_frame_opt(&mut s).await {
        assert_ne!(v["id"], delivery_id);
    }
}

#[tokio::test]
async fn delivery_failure_injection_rolls_back_and_says_zero() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.state.probe.fail_reply_log.store(true, Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "inj",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn noreply_empty_failed_make_zero_say() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "nr",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn startup_recover_stale_sending() {
    let db = opencrab_db::Db::memory().unwrap();
    {
        let conn = db.lock().unwrap();
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
        let sid: i64 = conn
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id='agent-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO gate_instances (
                instance_id, kind_id, subject_id, revision, enabled,
                config_b64, config_digest, created_at, updated_at
             ) VALUES ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa','k',?1,1,1,'e30=','bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',1,1)",
            [sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
             VALUES ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb','aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa','a',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deliveries (delivery_id, binding_id, payload_json, state, error, created_at, updated_at)
             VALUES ('cccccccc-cccc-cccc-cccc-cccccccccccc','bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb','{\"text\":\"x\"}','sending',NULL,1,1)",
            [],
        )
        .unwrap();
    }
    {
        let mut conn = db.lock().unwrap();
        recover_stale_deliveries(&mut conn, 99).unwrap();
        let (st, err): (String, String) = conn
            .query_row(
                "SELECT state, error FROM deliveries WHERE delivery_id='cccccccc-cccc-cccc-cccc-cccccccccccc'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(st, "indeterminate");
        assert_eq!(err, "stale sending recovered after restart");
    }
}

#[tokio::test]
async fn e2e_omoikane_flow() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "omoikane").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let acked = ack_bind(&mut s).await;
    assert_eq!(acked, binding_id);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "omo-1",
            "author_id": "user-1",
            "text": "hello",
            "attachments": []
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["seq"], 1);
    let mut saw_started = false;
    let mut saw_ended = false;
    let mut saw_say = false;
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(&mut s).await {
            match v["m"].as_str() {
                Some("activity") if v["state"] == "started" => saw_started = true,
                Some("activity") if v["state"] == "ended" => saw_ended = true,
                Some("say") => {
                    assert_eq!(v["payload"]["text"], "hello from agent");
                    write_frame(&mut s, &json!({"id": v["id"], "m": "ok"})).await;
                    saw_say = true;
                }
                _ => {}
            }
        }
        if saw_started && saw_ended && saw_say {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_started && saw_ended && saw_say);
}

#[tokio::test]
async fn hello_timeout_is_protocol_order() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    let mut leftover = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut leftover))
        .await
        .expect("hello timeout did not close")
        .expect("read after hello timeout");
    assert!(
        leftover.is_empty(),
        "id 未抽出の hello timeout は err frame 0: {leftover:?}"
    );
}

#[tokio::test]
async fn bind_timeout_is_bind_failed() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["m"], "bind");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn bind_err_closes() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    write_frame(
        &mut s,
        &json!({
            "id": bind["id"],
            "m": "err",
            "code": "bind_failed",
            "detail": null
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn running_unknown_message_keeps_connection() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id":"x","m":"edited"})).await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "unknown_message");
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "after",
            "author_id": "u",
            "text": "still",
            "attachments": []
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["m"], "ok");
}

#[tokio::test]
async fn running_reverse_and_unknown_without_id_keep() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "m": "activity",
            "binding_id": binding_id,
            "activity_id": uuid(),
            "state": "started"
        }),
    )
    .await;
    write_frame(&mut s, &json!({"m": "foo"})).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s-keep",
            "m": "said",
            "binding_id": binding_id,
            "origin": "after-keep",
            "author_id": "u",
            "text": "still",
            "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s-keep").await;
    assert_eq!(ok["m"], "ok");
}

#[tokio::test]
async fn malformed_response_is_response_invalid_and_closes() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id": "ghost", "m": "ok", "seq": "bad"})).await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "response_invalid");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn err_without_detail_is_response_invalid() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({"id": "ghost", "m": "err", "code": "external_rejected"}),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "response_invalid");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn listen_socket_rejects_relative_and_nonsocket() {
    assert_eq!(validate_listen_socket("").unwrap(), None);
    assert!(validate_listen_socket("relative/path.sock").is_err());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-socket");
    std::fs::write(&file, b"x").unwrap();
    assert!(validate_listen_socket(file.to_str().unwrap()).is_err());
}

#[tokio::test]
async fn lookups_unknown_address_is_false() {
    let h = Harness::start().await;
    let conn = h.state.db.lock().unwrap();
    assert!(!opencrab_extgate::channel_whitelisted(
        &conn, "agent-1", "missing", "nope"
    ));
    let _ = TRUSTED_PLATFORM_EXTGATE;
    let _ = session_id_for_binding("x");
}

#[tokio::test]
async fn closed_instance_put_does_not_revive() {
    let h = Harness::start().await;
    let id = uuid();
    put_instance(&h, &id, true).await;
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "discord",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");
}

#[tokio::test]
async fn address_in_use_and_binding_closed_reuse() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let a = uuid();
    let b = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &a, &instance_id, "same").await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{b}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"instance_id": instance_id, "address": "same"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "address_in_use");
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-bindings/{a}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{a}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"instance_id": instance_id, "address": "same"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "binding_closed");
}

const TOOL_DRIVEN_B64: &str = "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==";

fn tool_driven_digest() -> String {
    opencrab_extgate::ids::config_digest_from_b64(TOOL_DRIVEN_B64).unwrap()
}

async fn put_instance_config(h: &Harness, instance_id: &str, config_b64: &str) -> Value {
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
                        "enabled": true,
                        "config_b64": config_b64,
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

async fn hello_ok_digest(s: &mut UnixStream, instance_id: &str, revision: u64, digest: &str) {
    write_frame(
        s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": revision,
            "config_digest": digest,
        }),
    )
    .await;
    let ok = read_frame(s).await;
    assert_eq!(ok["m"], "ok");
    assert_eq!(ok["id"], "h1");
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

#[tokio::test]
async fn binding_put_reuses_existing_session_and_said_writes_there() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    insert_named_session(&h, "nostr-agent-1");
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "nostr-agent-1").await;
    {
        let conn = h.state.db.lock().unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        assert!(opencrab_db::queries::get_session(&conn, "nostr-agent-1")
            .unwrap()
            .is_some());
        assert!(
            opencrab_db::queries::get_session(&conn, &session_id_for_binding(&binding_id))
                .unwrap()
                .is_none()
        );
    }
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
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "reuse-1",
            "author_id": "u1",
            "text": "hello reuse",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["seq"], 1);
    let conn = h.state.db.lock().unwrap();
    let session_id: String = conn
        .query_row(
            "SELECT session_id FROM memory_sessions WHERE content = 'hello reuse'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session_id, "nostr-agent-1");
}

#[tokio::test]
async fn binding_put_reuse_membership_mismatch_conflicts() {
    let h = Harness::start().await;
    {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "agent-2".into(),
                name: "B".into(),
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
        opencrab_db::queries::insert_session(
            &conn,
            &SessionRow {
                id: "owned-by-2".into(),
                mode: "solo".into(),
                theme: "x".into(),
                phase: "convergent".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: r#"["agent-2"]"#.into(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"instance_id": instance_id, "address": "owned-by-2"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "binding_conflict");
    let conn = h.state.db.lock().unwrap();
    let bindings: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bindings, 0);
}

#[tokio::test]
async fn tool_driven_inbound_is_no_reply_without_say() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance_config(&h, &instance_id, TOOL_DRIVEN_B64).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok_digest(&mut s, &instance_id, 1, &tool_driven_digest()).await;
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
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "td1",
            "author_id": "u1",
            "text": "ask",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["seq"], 1);
    for _ in 0..50 {
        if h.runtime.turns.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(80)).await;
    while let Some(frame) = read_frame_opt(&mut s).await {
        assert_ne!(frame["m"], "say", "{frame}");
    }
    let conn = h.state.db.lock().unwrap();
    let deliveries: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(deliveries, 0);
    let no_reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE content = 'NO_REPLY'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(no_reply, 1);
}

#[tokio::test]
async fn missing_delivery_mode_keeps_say() {
    assert_eq!(
        opencrab_extgate::delivery_mode_from_config_bytes(b"{}").unwrap(),
        opencrab_extgate::DeliveryMode::Say
    );
    assert!(opencrab_extgate::dispatches_v3_say(
        opencrab_extgate::DeliveryMode::Say
    ));
    assert!(!opencrab_extgate::dispatches_v3_say(
        opencrab_extgate::DeliveryMode::ToolDriven
    ));
}

/// QC 893e4e98: watch Bundle の 1 件目 Accepted が pending_turn を握り、
/// 後続 member が Busy → coordinator が all_in 待ちのまま turn が無い。
/// record のあと間隔を進め、receipt で全 member を通すと turn が発火する。
#[tokio::test(start_paused = true)]
async fn watch_bundle_records_then_fires_turn_after_interval() {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicU32;

    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    let address = "nostr-agent-1";
    insert_named_session(&h, address);
    let watch_id = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_watch(&conn, address, "agent-1", 120, "{}").unwrap()
    };
    let author = "aa".repeat(32);
    let origins = [
        format!("nostr:event:v1:watch:{watch_id}:{}", "11".repeat(32)),
        format!("nostr:event:v1:watch:{watch_id}:{}", "22".repeat(32)),
        format!("nostr:event:v1:watch:{watch_id}:{}", "33".repeat(32)),
    ];
    let idx = AtomicU32::new(0);
    let origins_hook = origins.to_vec();
    h.state.set_nostr_said_admit(Arc::new(move |_, _, _| {
        let i = idx.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(NostrSaidDecision::Accept {
            watch_id: Some(watch_id),
            immediate: false,
            bundle: Some(NostrBundleAdmit {
                bundle_id: "d781a3e7eaacca5606f44e87025f0503c12cf273ed33834fe6de37b6f55ad6bd"
                    .into(),
                index: i,
                count: 3,
                origins: origins_hook.clone(),
            }),
        })
    }));
    let follow_key = author.clone();
    h.state.set_nostr_watch_sets(Arc::new(move |_| {
        Some(NostrWatchSets {
            followees: HashSet::from([follow_key.clone()]),
            owner: HashSet::new(),
            co_agents: HashSet::new(),
            trusted_users: HashSet::new(),
        })
    }));

    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "nostr",
                        "subject_id": h.subject_id,
                        "enabled": true,
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
    put_binding(&h, &binding_id, &instance_id, address).await;

    let client = InstanceClient::connect(
        &h.sock,
        instance_id.clone(),
        1,
        author.clone(),
        config_digest(),
    )
    .await
    .expect("connect");
    let mut bound = false;
    for _ in 0..80 {
        if client.binding_for_address(address).await.as_deref() == Some(binding_id.as_str()) {
            bound = true;
            break;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
    }
    assert!(bound, "bind ack");

    tokio::time::advance(Duration::from_secs(120)).await;

    let before_logs = {
        let conn = h.state.db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
            [address],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    let before_turns = h.runtime.turns.load(Ordering::SeqCst);
    let members_json = serde_json::to_string(&origins).unwrap();
    let event_ids = ["11".repeat(32), "22".repeat(32), "33".repeat(32)];

    for (i, origin) in origins.iter().enumerate() {
        let text = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":true,\"bundle_id\":\"d781a3e7eaacca5606f44e87025f0503c12cf273ed33834fe6de37b6f55ad6bd\",\"count\":3,\"event_id\":\"{}\",\"has_e\":false,\"index\":{},\"kind\":1,\"p_self\":false,\"route\":\"bundle\",\"watch_id\":{watch_id}}}]\n[NOSTRBUNDLE/V1 {members_json}]\nいかかつ東京\n[Nostr kind:1 メンション from={author} target=note1x]",
            event_ids[i],
            i + 1
        );
        let outcome = client
            .post_said_receipt(address, origin, &author, &text, &[])
            .await
            .unwrap_or_else(|e| panic!("member {} refuse {e:?}", i + 1));
        assert!(
            matches!(outcome, SaidOutcome::Accepted { .. }),
            "member {} {outcome:?}",
            i + 1
        );
        if i == 0 {
            let logs = {
                let conn = h.state.db.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                    [address],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
            };
            assert!(logs > before_logs, "first member records");
            assert_eq!(
                h.runtime.turns.load(Ordering::SeqCst),
                before_turns,
                "incomplete bundle does not fire"
            );
        }
    }

    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) > before_turns {
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert!(
        h.runtime.turns.load(Ordering::SeqCst) > before_turns,
        "all receipts enqueue a turn"
    );
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

fn session_log_count(h: &Harness, session_id: &str) -> i64 {
    let conn = h.state.db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// turn 実行中に届いた said が消えず、turn 終了後に処理される。
#[tokio::test(start_paused = true)]
async fn said_during_turn_is_recorded_and_runs_after() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let session_id = format!("extgate-{binding_id}");
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);

    let client = InstanceClient::connect(&h.sock, instance_id, 1, "u1".into(), config_digest())
        .await
        .expect("connect");
    wait_client_bound(&client, "chan-1", &binding_id).await;

    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);
    let first = client
        .post_said("chan-1", "origin-1", "first", &[])
        .await
        .unwrap_or_else(|e| panic!("first refuse {e:?}"));
    assert!(
        matches!(first, SaidOutcome::Accepted { seq: 1 }),
        "{first:?}"
    );
    for _ in 0..80 {
        tokio::select! {
            _ = &mut entered => break,
            _ = async {
                tokio::time::advance(Duration::from_millis(5)).await;
            } => {}
        }
    }

    let second = client
        .post_said("chan-1", "origin-2", "second-during-turn", &[])
        .await
        .unwrap_or_else(|e| panic!("second refuse {e:?}"));
    assert!(
        matches!(second, SaidOutcome::Accepted { seq: 2 }),
        "said during turn must be accepted, got {second:?}"
    );
    assert_eq!(session_log_count(&h, &session_id), 2);
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        0,
        "second turn waits"
    );

    release_tx.send(()).unwrap();
    for _ in 0..80 {
        if h.runtime.turns.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        2,
        "queued said runs after the held turn"
    );
}

/// キュー満杯は seq=null で拒否し、履歴に残さない。
#[tokio::test]
async fn session_queue_overflow_is_seq_null_and_counted() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = format!("extgate-{binding_id}");
    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);

    for i in 0..32 {
        let id = format!("s{i}");
        write_frame(
            &mut s,
            &json!({
                "id": id,
                "m": "said",
                "binding_id": binding_id,
                "origin": format!("o-{i}"),
                "author_id": "u1",
                "text": format!("m{i}"),
                "attachments": []
            }),
        )
        .await;
        let v = read_said_response(&mut s, &id).await;
        assert_eq!(v["m"], "ok", "said {i} {v}");
        assert_eq!(v["seq"], i + 1, "said {i} {v}");
    }
    write_frame(
        &mut s,
        &json!({
            "id": "sover",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-overflow",
            "author_id": "u1",
            "text": "too-many",
            "attachments": []
        }),
    )
    .await;
    let overflow = read_said_response(&mut s, "sover").await;
    assert_eq!(overflow["m"], "ok");
    assert!(overflow["seq"].is_null(), "{overflow}");
    assert_eq!(session_log_count(&h, &session_id), 32);
    assert!(h.state.turn_queues.dropped() >= 1);
    assert!(h.state.probe.turn_queue_dropped.load(Ordering::SeqCst) >= 1);
    let _ = release_tx.send(());
}

async fn wait_turns(h: &Harness, n: usize) {
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if h.runtime.turns.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        got.is_ok(),
        "expected {n} turns, got {}",
        h.runtime.turns.load(Ordering::SeqCst)
    );
}

/// 遅いツール相当（`tool_hold`）を解放しなくても turn が終わる。
/// 修正前は sink 無し → `tool_hold` を待つのでこのテストは赤。
#[tokio::test]
async fn v3_turn_returns_before_slow_tool_finishes() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_tool_tx, tool_rx) = oneshot::channel();
    *h.runtime.tool_hold_rx.lock().unwrap() = Some(tool_rx);

    write_frame(
        &mut s,
        &json!({
            "id": "slow1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-slow",
            "author_id": "u1",
            "text": "sleep 120 を実行して",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "slow1").await;
    assert_eq!(v["m"], "ok", "{v}");

    wait_turns(&h, 1).await;
    assert!(
        h.runtime.sink_seen.load(Ordering::SeqCst),
        "V3 RunRequest に completion_sink が付いていること"
    );
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
}

/// ツール実行中（`tool_hold` 未解放）に届いた said が次の turn を起こせる。
#[tokio::test]
async fn said_during_detached_tool_starts_next_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (_tool_tx, tool_rx) = oneshot::channel();
    *h.runtime.tool_hold_rx.lock().unwrap() = Some(tool_rx);

    write_frame(
        &mut s,
        &json!({
            "id": "d1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-1",
            "author_id": "u1",
            "text": "sleep 120",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "d1").await["m"], "ok");
    wait_turns(&h, 1).await;

    write_frame(
        &mut s,
        &json!({
            "id": "d2",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-2",
            "author_id": "u1",
            "text": "追いメンション",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "d2").await["m"], "ok");
    wait_turns(&h, 2).await;
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        2,
        "ツール完了を待たずに次 turn が走ること"
    );
}

/// 決着本文は DB に着地したあと、resume の 1 turn で会話に載る。
#[tokio::test]
async fn settlement_is_consumed_on_next_turn() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    let session_id = format!("extgate-{binding_id}");

    write_frame(
        &mut s,
        &json!({
            "id": "c1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o-1",
            "author_id": "u1",
            "text": "sleep 120",
            "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "c1").await["m"], "ok");
    wait_turns(&h, 1).await;

    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id: binding_id.clone(),
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        kind_id: "discord".into(),
        author_id: "u1".into(),
        delivery_mode: DeliveryMode::Say,
        prompt_suffix: String::new(),
    };
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-sleep".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"ok":true,"slept":120}"#,
    );
    wait_turns(&h, 2).await;

    let resume_conv = h
        .runtime
        .conversations
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        resume_conv.contains("subtask_completed") && resume_conv.contains("slept"),
        "決着結果が次 turn の会話に載ること: {resume_conv}"
    );
}

/// **#838 row284 の穴を塞ぐ本命**。決着を fake せず `settle_completed`（＝実経路の
/// `dispatch_settled`）を通し、session_id が **`extgate-` 接頭辞でない再利用セッション**
/// （Nostr は `canonical_session_id` が address = `nostr-<agent_id>` へフォールバックする）
/// でも `ExtgateCompletionSink::deliver_continuation` → resume turn が起きることを検証する。
///
/// 旧実装は `dispatch_settled` が `ev.session_id.starts_with("extgate-")` で親判定していたため、
/// `nostr-agent-1` は門前払いされ resume が一切起きなかった（このテストは turns==0 のまま落ちる）。
/// 既存の `settlement_is_consumed_on_next_turn` は `extgate-{binding_id}` を使うため接頭辞判定を
/// 素通りし、この穴を踏めていなかった。
#[tokio::test]
async fn settlement_on_reused_nostr_session_resumes() {
    let h = Harness::start().await;
    // 連結済みの instance/binding を用意する（Say 配送の送出先）。ただし決着させる親
    // セッションは binding の canonical（extgate-…）ではなく、Nostr 再利用の address 形式。
    let (_s, instance_id, binding_id) = ready_pair(&h).await;
    let session_id = "nostr-agent-1".to_string();
    assert!(
        !session_id.starts_with("extgate-"),
        "テスト前提: session が extgate- 接頭辞でないこと"
    );

    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id,
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        kind_id: "nostr".into(),
        author_id: "npub-u1".into(),
        delivery_mode: DeliveryMode::Say,
        prompt_suffix: String::new(),
    };
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-sleep".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"ok":true,"slept":30}"#,
    );

    // resume turn が実際に走ること（旧実装なら guard に阻まれ turns は 0 のまま）。
    wait_turns(&h, 1).await;
    assert_eq!(
        h.runtime.turns.load(Ordering::SeqCst),
        1,
        "nostr- 再利用セッションの決着でも resume turn が 1 回走ること"
    );
    let resume_conv = h
        .runtime
        .conversations
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        resume_conv.contains("subtask_completed") && resume_conv.contains("slept"),
        "決着結果が resume turn の会話に載ること: {resume_conv}"
    );
}

// ===== DI 拡張: 能力宣言 hello・digest・invoke 往復（§3/§5/§8・実経路） =====

/// hello に載せる最小 operations（reply 1 件・§9.2 スキーマ形・name 昇順）。
fn ops_reply() -> Value {
    json!([{
        "name": "reply",
        "description": "reply to an event",
        "input_schema": {"type": "object", "required": ["event", "text"],
            "properties": {"event": {"type": "string"}, "text": {"type": "string"}}},
        "output_schema": null,
        "callback_schema": null,
        "class": {"sub_engine": "not_exposed", "sharing": "conversation_bound"}
    }])
}

async fn wait_acked(h: &Harness, instance_id: &str, binding_id: &str) {
    for _ in 0..200 {
        let acked = h
            .state
            .lock_registry()
            .unwrap()
            .get(instance_id)
            .is_some_and(|e| e.acknowledged.contains(binding_id));
        if acked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("binding {binding_id} not acknowledged in time");
}

async fn hello_with_ops(
    s: &mut UnixStream,
    instance_id: &str,
    revision: u64,
    ops: &Value,
) -> Value {
    write_frame(
        s,
        &json!({
            "id": "h1", "m": "hello", "protocol": 2, "instance_id": instance_id,
            "revision": revision, "config_digest": config_digest(), "operations": ops,
        }),
    )
    .await;
    read_frame(s).await
}

/// hello + operations が受理され、instance に digest が永続する（§3.3/DI-04）。
#[tokio::test]
async fn di_hello_operations_accepted_and_digest_persisted() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    let ok = hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await;
    assert_eq!(ok["m"], "ok", "operations つき hello が受理される");
    // digest が gate_instances に永続している。
    let digest: Option<String> = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT operation_declaration_digest FROM gate_instances WHERE instance_id = ?1",
            [&instance_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        digest.is_some_and(|d| d.len() == 64),
        "宣言 digest が永続する"
    );
}

/// 同一 revision で宣言 digest が変わった再接続は operation_declaration_mismatch（DI-04）。
#[tokio::test]
async fn di_hello_operations_digest_mismatch_on_reconnect() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    // 初回 hello で digest 確立。
    let mut s1 = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s1, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    drop(s1);
    // 別宣言（reaction 1 件）で同一 revision に再接続 → mismatch + close。
    let other = json!([{
        "name": "reaction", "description": "d",
        "input_schema": {"type": "object"}, "output_schema": null, "callback_schema": null,
        "class": {"sub_engine": "not_exposed", "sharing": "conversation_bound"}
    }]);
    let mut s2 = h.connect().await;
    let resp = hello_with_ops(&mut s2, &instance_id, 1, &other).await;
    assert_eq!(resp["m"], "err");
    assert_eq!(resp["code"], "operation_declaration_mismatch");
}

/// 不正な宣言（非 sort）は operation_declaration_invalid + close（DI-22）。
#[tokio::test]
async fn di_hello_invalid_operations_rejected() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    // name 逆順（非 sort）。
    let bad = json!([
        {"name": "reply", "description": "d", "input_schema": {"type": "object"},
         "output_schema": null, "callback_schema": null,
         "class": {"sub_engine": "not_exposed", "sharing": "conversation_bound"}},
        {"name": "follow", "description": "d", "input_schema": {"type": "object"},
         "output_schema": null, "callback_schema": null,
         "class": {"sub_engine": "not_exposed", "sharing": "agent_bound"}}
    ]);
    let mut s = h.connect().await;
    let resp = hello_with_ops(&mut s, &instance_id, 1, &bad).await;
    assert_eq!(resp["m"], "err");
    assert_eq!(resp["code"], "operation_declaration_invalid");
}

/// invoke 往復（成功）: core invoke_and_wait → gateway ok(result) → Ok(result) + call succeeded。
#[tokio::test]
async fn di_invoke_round_trip_success() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-di").await;
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    let acked = ack_bind(&mut s).await;
    assert_eq!(acked, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    // core 側で invoke を発行（背景 subtask 相当）。
    let state = Arc::clone(&h.state);
    let inst = instance_id.clone();
    let bind = binding_id.clone();
    let call = tokio::spawn(async move {
        invoke_and_wait(
            &state,
            &inst,
            &bind,
            "reply",
            &json!({"event": "e1", "text": "hi"}),
        )
        .await
    });

    // gateway 側: invoke を受けて ok(result) を返す。
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "reply");
    assert_eq!(invoke["payload"]["text"], "hi");
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "ok", "result": {"posted": true}}),
    )
    .await;

    let out = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("invoke_and_wait 完了")
        .unwrap();
    assert_eq!(out.unwrap(), json!({"posted": true}), "result が返る");
    // call row が succeeded。
    let state_str: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_str, "succeeded");
}

/// invoke 往復（確定拒否）: gateway err(operation_rejected) → Err + call failed。
#[tokio::test]
async fn di_invoke_round_trip_rejected() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-di2").await;
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    let state = Arc::clone(&h.state);
    let inst = instance_id.clone();
    let bind = binding_id.clone();
    let call = tokio::spawn(async move {
        invoke_and_wait(
            &state,
            &inst,
            &bind,
            "reply",
            &json!({"event": "e1", "text": "x"}),
        )
        .await
    });
    let invoke = read_until(&mut s, |v| v["m"] == "invoke").await;
    let call_id = invoke["id"].as_str().unwrap().to_string();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "err", "code": "operation_rejected", "detail": null}),
    )
    .await;
    let out = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("完了")
        .unwrap();
    assert!(out.is_err(), "確定拒否は Err");
    let state_str: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_str, "failed");
}

/// 宣言に無い operation の invoke は call/wire 0 で operation_unknown（§5.1）。
#[tokio::test]
async fn di_invoke_undeclared_operation_is_unknown() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-di3").await;
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;
    // "repost" は宣言していない。
    let out = invoke_and_wait(&h.state, &instance_id, &binding_id, "repost", &json!({})).await;
    assert!(out.is_err(), "未宣言 operation は Err");
    // call row は作られない。
    let count: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "未宣言 invoke は call row 0");
}
