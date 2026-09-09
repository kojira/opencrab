use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use opencrab_llm::message::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;
use opencrab_server::AppState;

use opencrab_extgate::{
    admin_router, resolve_caller_identity_with_owner, serve_uds, ExtgateState, NostrSaidDecision,
    NostrWatchSets, OperatorToken,
};
use opencrab_gate_client::client::InstanceClient;
use opencrab_nostr_gateway::config::InstancePlacement;
use opencrab_nostr_gateway::harness::HarnessOverrides;
use opencrab_nostr_gateway::run::spawn_instance;

use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

const TOKEN: &str = "operator-token-qc";
const AGENT_ID: &str = "agent-qc";
/// dry-run say を拾う tracing target（= `opencrab_nostr_gateway::post::DRY_RUN_LOG_TARGET`）。
const DRY_RUN_TARGET: &str = "opencrab_nostrgate::dry_run";
/// NO_REPLY 破棄ログを拾う tracing target（= `opencrab_actions::no_reply::NO_REPLY_LOG_TARGET`）。
const NO_REPLY_TARGET: &str = "opencrab::no_reply";

fn self_pk() -> String {
    "11".repeat(32)
}
fn author_pk() -> String {
    "22".repeat(32)
}

// ==================== 観測: dry-run say キャプチャ ====================

#[derive(Clone, Debug, Default)]
struct CapturedSay {
    kind: String,
    body: String,
}

/// NO_REPLY 破棄ログ（`no_reply_trailing_discarded` WARN）の観測。
#[derive(Clone, Debug, Default)]
struct CapturedDiscard {
    discarded: String,
    session_id: String,
}

static BUFFER: OnceLock<Arc<Mutex<Vec<CapturedSay>>>> = OnceLock::new();
static DISCARD_BUFFER: OnceLock<Arc<Mutex<Vec<CapturedDiscard>>>> = OnceLock::new();
static INIT: Once = Once::new();

/// グローバル subscriber を 1 回だけ張り、共有バッファを返す。
///
/// tracing の thread-local `with_default` は `tokio::spawn` の別スレッドへ伝播しないため、
/// dry-run ログ（consumer タスクが別スレッドで吐く）を拾うにはグローバル default が必須。
fn install_capture() -> Arc<Mutex<Vec<CapturedSay>>> {
    let buf = BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    let discard = DISCARD_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    INIT.call_once(|| {
        let layer = CaptureLayer {
            buf: buf.clone(),
            discard,
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        // 既に別の default が張られていても壊さない（best-effort）。
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    buf
}

/// 破棄ログの共有バッファ（`install_capture` が張った後に読む）。
fn discard_buffer() -> Arc<Mutex<Vec<CapturedDiscard>>> {
    DISCARD_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

struct CaptureLayer {
    buf: Arc<Mutex<Vec<CapturedSay>>>,
    discard: Arc<Mutex<Vec<CapturedDiscard>>>,
}

#[derive(Default)]
struct SayVisitor {
    kind: Option<String>,
    body: Option<String>,
}

impl SayVisitor {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "kind" => self.kind = Some(value),
            "body" => self.body = Some(value),
            _ => {}
        }
    }
}

impl tracing::field::Visit for SayVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // deliver_say は body を `%`（Display）で出す。Debug ラッパの `{:?}` は Display
        // 文字列そのまま（引用符なし）になる。
        self.set(field.name(), format!("{value:?}"));
    }
}

/// `no_reply_trailing_discarded` WARN の構造化フィールドを拾う。
#[derive(Default)]
struct DiscardVisitor {
    discarded: Option<String>,
    session_id: Option<String>,
}

impl DiscardVisitor {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "discarded" => self.discarded = Some(value),
            "session_id" => self.session_id = Some(value),
            _ => {}
        }
    }
}

impl tracing::field::Visit for DiscardVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        match event.metadata().target() {
            DRY_RUN_TARGET => {
                let mut v = SayVisitor::default();
                event.record(&mut v);
                self.buf.lock().unwrap().push(CapturedSay {
                    kind: v.kind.unwrap_or_default(),
                    body: v.body.unwrap_or_default(),
                });
            }
            NO_REPLY_TARGET => {
                let mut v = DiscardVisitor::default();
                event.record(&mut v);
                self.discard.lock().unwrap().push(CapturedDiscard {
                    discarded: v.discarded.unwrap_or_default(),
                    session_id: v.session_id.unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
}

fn captured(buf: &Arc<Mutex<Vec<CapturedSay>>>) -> Vec<CapturedSay> {
    buf.lock().unwrap().clone()
}

fn discards(buf: &Arc<Mutex<Vec<CapturedDiscard>>>) -> Vec<CapturedDiscard> {
    buf.lock().unwrap().clone()
}

/// 述語が真になるまで最大 ~5s ポーリングする。
async fn wait_until(pred: impl Fn() -> bool) -> bool {
    for _ in 0..250 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

fn body_index(buf: &Arc<Mutex<Vec<CapturedSay>>>, needle: &str) -> Option<usize> {
    captured(buf).iter().position(|c| c.body.contains(needle))
}

// ==================== fixture ====================

struct Fixture {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watch.jsonl");
        std::fs::write(&path, "").unwrap();
        Self { path, _dir: dir }
    }

    fn append_line(&self, line: &str) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }
}

/// 自分宛て `#p` の kind:1 メンション 1 行（WatchEvent JSONL）。`content` にルーティング用の
/// マーカーを載せる（extgate は V1 アンカーを剥がした本文＝この content を会話へ記録する）。
fn mention_event(id: &str, content: &str) -> String {
    serde_json::json!({
        "id": id,
        "pubkey": author_pk(),
        "npub": null,
        "note_id": null,
        "created_at": 1i64,
        "kind": 1,
        "content": content,
        "tags": [["p", self_pk()]],
    })
    .to_string()
}

// ==================== FIFO mock（単発ターン用: (a)/(c)） ====================

struct FifoMock {
    responses: Mutex<std::collections::VecDeque<ChatResponse>>,
    system_prompts: Mutex<Vec<String>>,
}

impl FifoMock {
    fn new() -> Self {
        Self {
            responses: Mutex::new(std::collections::VecDeque::new()),
            system_prompts: Mutex::new(Vec::new()),
        }
    }
    fn push_text(&self, text: &str) {
        self.responses
            .lock()
            .unwrap()
            .push_back(text_response(text));
    }
    fn system_prompts(&self) -> Vec<String> {
        self.system_prompts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for FifoMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.system_prompts
            .lock()
            .unwrap()
            .push(system_of(&request));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("FifoMock: no more queued responses"))
    }
}

// ==================== 共通 helpers ====================

fn system_of(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 全メッセージ本文を連結（ルーティング用の会話マーカー検出に使う）。
fn request_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_tool_role(request: &ChatRequest) -> bool {
    request.messages.iter().any(|m| m.role == Role::Tool)
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: Message::assistant(text),
            finish_reason: Some(FinishReason::Stop),
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        created: 0,
    }
}

fn tool_call_response(name: &str, args: serde_json::Value) -> ChatResponse {
    tool_calls_response(vec![(name, args)])
}

fn tool_calls_response(calls: Vec<(&str, serde_json::Value)>) -> ChatResponse {
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(
            calls
                .into_iter()
                .map(|(name, args)| ToolCall {
                    id: format!("tc-{}", uuid::Uuid::new_v4()),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: name.to_string(),
                        arguments: args.to_string(),
                    },
                })
                .collect(),
        ),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

/// mock モデルの予算 envelope を満たす（#826 fail-loud 対策）。
fn register_mock_pricing(db: &opencrab_db::Db) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: "mock".to_string(),
            model: "gpt-4o".to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(200_000),
            max_input_tokens: Some(200_000),
            max_output_tokens: Some(4_096),
            max_total_tokens: None,
        },
    )
    .expect("test model_pricing");
}

fn upsert_test_agent(db: &opencrab_db::Db) -> i64 {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT_ID.into(),
            name: "QC".into(),
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
        "SELECT subject_id FROM agents WHERE agent_id = ?1",
        [AGENT_ID],
        |r| r.get(0),
    )
    .unwrap()
}

struct MeteredHarnessProvider(Arc<dyn LlmProvider>);

#[async_trait::async_trait]
impl LlmProvider for MeteredHarnessProvider {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn sends_max_output_tokens(&self) -> bool {
        self.0.sends_max_output_tokens()
    }

    fn measure_request_tokens(&self, request: &ChatRequest) -> Option<usize> {
        serde_json::to_vec(request).ok().map(|wire| wire.len())
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        self.0.available_models().await
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.0.chat_completion(request).await
    }
}

fn build_app_state(db: opencrab_db::Db, provider: Arc<dyn LlmProvider>) -> AppState {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(MeteredHarnessProvider(provider)));

    router.set_default_provider("mock");
    AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir()
            .join("opencrab_qc_harness")
            .to_string_lossy()
            .to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        typed_history_enabled: false,
        typed_history_drop_directive: false,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: Default::default(),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(Default::default()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    }
}

struct Core {
    extgate: Arc<ExtgateState>,
    state: AppState,
    sock: PathBuf,
    subject_id: i64,
    _dir: tempfile::TempDir,
    _ws: tempfile::TempDir,
}

/// 実 serve_uds core + 実 AppState runtime を UDS で立ち上げ、nostr admit/watch hooks を配線する。
async fn start_core(provider: Arc<dyn LlmProvider>) -> Core {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    register_mock_pricing(&db);
    let subject_id = upsert_test_agent(&db);
    // owner = 発端 author にして caller=Owner に解決させる（spawn_subtask 等を確実に使える）。
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: AGENT_ID.into(),
                secret_key: "nsec1placeholder".into(),
                relays_json: "[]".into(),
                filter_json: "{}".into(),
                enabled: true,
            },
        )
        .unwrap();
        opencrab_db::queries::set_agent_nostr_owner_pubkey(&conn, AGENT_ID, &author_pk()).unwrap();
    }

    let extgate = Arc::new(ExtgateState::new(
        db.clone(),
        OperatorToken::from_bytes(TOKEN),
    ));

    // nostr said の元栓。production（server main）と同じ `admit_nostr_said` を呼ぶ。
    // allow-set に author を入れ、self_pubkey は config と揃える。
    let self_pk = self_pk();
    let author = author_pk();
    extgate.set_nostr_said_admit(Arc::new(move |_agent_id, author_id, text| {
        use opencrab_extgate::{ErrorCode, GateError};
        use opencrab_nostr::{admit_nostr_said, AdmitSaidError, AllowSources, IngressRoute};
        let mut allow = AllowSources::default();
        allow.owner.insert(author.clone());
        match admit_nostr_said(text, author_id, &self_pk, &allow) {
            Err(AdmitSaidError::BadAnchor) => Err(GateError::new(ErrorCode::BadRequest)),
            Err(AdmitSaidError::Drop { .. }) => Ok(NostrSaidDecision::Drop { bundle: None }),
            Ok(anchor) => Ok(NostrSaidDecision::Accept {
                watch_id: anchor.watch_id,
                immediate: anchor.route == IngressRoute::Immediate,
                bundle: None,
            }),
        }
    }));
    let author_sets = author_pk();
    extgate.set_nostr_watch_sets(Arc::new(move |_agent_id| {
        let mut sets = NostrWatchSets::default();
        sets.owner.insert(author_sets.clone());
        Some(sets)
    }));
    let ws = tempfile::tempdir().unwrap();
    let ws_path = ws.path().to_path_buf();
    // workspace hook はサニタイズ退避先。TempDir は Core が保持して test 期間中は生かす。
    extgate.set_nostr_workspace(Arc::new(move |_agent_id| Some(ws_path.clone())));
    extgate.set_nostr_relay(Arc::new(|_agent_id, _text| {}));

    let state = build_app_state(db.clone(), provider);
    // #925: 本番と同じ descriptor 登録＋ V3 heartbeat 受け口を実型で配線する（Nostr レーンも
    // canonical session は `extgate-<binding_id>` で同一 descriptor が受ける）。未登録なら
    // resolve_target None で配送 0＝赤。
    opencrab_server::register_production_descriptors(&state.timed_fire_router);
    state.timed_fire_router.register_shared(
        opencrab_extgate::EXTGATE_TIMED_FIRE_KIND,
        Arc::new(opencrab_extgate::ExtgateTimedFireSink::new(
            extgate.clone(),
            state.clone(),
        )),
    );

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    {
        let listen_state = Arc::clone(&extgate);
        let runtime = state.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(
                listen_state,
                runtime,
                resolve_caller_identity_with_owner,
                path,
            )
            .await;
        });
    }
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    Core {
        extgate,
        state,
        sock,
        subject_id,
        _dir: dir,
        _ws: ws,
    }
}

async fn admin(core: &Core, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let app = admin_router(Arc::clone(&core.extgate));
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

async fn put_instance(core: &Core, instance_id: &str, config_b64: &str) {
    let (st, body) = admin(
        core,
        Request::builder()
            .method("PUT")
            .uri(format!("/api/gate-instances/{instance_id}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "kind_id": "nostr",
                    "subject_id": core.subject_id,
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
        "put_instance {st}: {}",
        String::from_utf8_lossy(&body)
    );
}

async fn put_binding(core: &Core, binding_id: &str, instance_id: &str, address: &str) {
    let (st, body) = admin(
        core,
        Request::builder()
            .method("PUT")
            .uri(format!("/api/gate-bindings/{binding_id}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"instance_id": instance_id, "address": address}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "put_binding {st}: {}",
        String::from_utf8_lossy(&body)
    );
}

/// instance + binding を登録し、`spawn_instance`（fake_watch＋dry_run）を起動して bind ack を待つ。
/// 返り値の `session_id` は canonical（新規 address なら `extgate-{binding_id}`）。
async fn wire_instance(
    core: &Core,
    fixture: &Fixture,
    config_bytes: Vec<u8>,
) -> (Arc<InstanceClient>, String, String) {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let address = format!("nostr-{AGENT_ID}");
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);

    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &address).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        address: address.clone(),
        config_b64,
    };
    let overrides = HarnessOverrides {
        fake_watch: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(
        core.sock.clone(),
        &place,
        &config_bytes,
        None,
        PathBuf::from("/nonexistent/nostaro"),
        overrides,
    )
    .expect("spawn_instance");

    // bind ack を待つ（watch lane はここから起動する）。
    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&address).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない");

    let session_id = format!("extgate-{binding_id}");
    (client, address, session_id)
}

fn nostr_config(watches: Option<serde_json::Value>) -> Vec<u8> {
    let mut cfg = serde_json::json!({
        "relays": ["wss://relay.invalid"],
        "self_pubkey": self_pk(),
        "name": "crab",
        "delivery_mode": "say",
    });
    if let Some(w) = watches {
        cfg["watches"] = w;
    }
    serde_json::to_vec(&cfg).unwrap()
}

