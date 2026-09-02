//! QC ハーネス E2E（Phase 2）。
//!
//! リレー・鍵・nostaro 子プロセス無しで、**実配線**を通す決定的オフライン E2E:
//!   実 `serve_uds`（extgate core）＋ 実 `AppState`（mock LLM）
//!     ⇕ 実 UDS ⇕
//!   実 `spawn_instance`（nostr-gateway・fake_watch＋dry_run 両有効）
//!
//! 観測channel = dry-run の tracing ログ（target = `opencrab_nostrgate::dry_run`）。
//! グローバル subscriber を 1 回だけ張り、各テストは注入した固有本文で絞る。
//! 単一スレッド（`--test-threads=1`）前提。
//!
//! 注意（現行本線 DI-16 / row292）: say は常に standalone post として publish される
//! （特定イベントへの e-tag 返信は DI `reply` 操作が担い、say 経路には返信先が無い）。
//! よって観測は「standalone post の本文」で行い、返信先イベント id では相関しない。

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
            max_output_tokens: Some(4_096),
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

fn build_app_state(db: opencrab_db::Db, provider: Arc<dyn LlmProvider>) -> AppState {
    let mut router = LlmRouter::new();
    router.add_provider(provider);
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

// ==================== (a) mention → say（standalone post） ====================

#[tokio::test]
async fn scenario_a_mention_becomes_say() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text("QCA-ACK 了解、やっておくね");
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let event_id = "a1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "QCA-MARK メンション本文"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.body.contains("QCA-ACK") && c.kind == "standalone")
        })
        .await
    };
    assert!(
        ok,
        "mention の say が dry-run に出ない: {:?}",
        captured(&buf)
    );

    // 1 メンション = 1 ターン。
    assert_eq!(mock.system_prompts().len(), 1, "ターンが 1 本でない");
}

// ============ (A1/A1L) NO_REPLY 終端化 + 破棄ログ（第一柱・DESIGN-RESUME-SETTLE §3.1/§3.1.1）============

/// mock 応答に `…本文… NO_REPLY …ゴミ…` を混入させたとき:
/// (i) 配送 say の body に `NO_REPLY` もゴミも含まれず、前段本文だけで確定する（A1）
/// (ii) 破棄ログ `no_reply_trailing_discarded` が 1 件出て破棄全文と session_id を持つ（A1L）
/// (iii) 破棄テキストは wire（dry-run say）に一切現れない（§3.1.1(c)）
#[tokio::test]
async fn scenario_no_reply_terminates_and_logs_discard() {
    // 互いに部分文字列にならない一意マーカー（グローバルバッファの他テスト混線を避ける）。
    const KEEP: &str = "NRTERM-KEEP 本文はここまで";
    const GARBAGE: &str = "NRTERM-GARBAGE 破棄されるゴミ";

    let buf = install_capture();
    let dbuf = discard_buffer();
    let mock = Arc::new(FifoMock::new());
    mock.push_text(&format!("{KEEP} NO_REPLY {GARBAGE}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let event_id = "d1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "NRTERM-MARK メンション本文"));

    // (i) 前段本文で say が確定するまで待つ。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.body.contains("NRTERM-KEEP") && c.kind == "standalone")
        })
        .await
    };
    assert!(ok, "前段本文の say が出ない: {:?}", captured(&buf));

    // (i) 該当 say の body に NO_REPLY もゴミも含まれない。
    let says: Vec<_> = captured(&buf)
        .into_iter()
        .filter(|c| c.body.contains("NRTERM-KEEP"))
        .collect();
    for s in &says {
        assert!(
            !s.body.contains("NO_REPLY"),
            "say body に NO_REPLY が混入: {:?}",
            s
        );
        assert!(
            !s.body.contains("NRTERM-GARBAGE"),
            "say body に破棄テキストが混入: {:?}",
            s
        );
    }

    // (iii) 破棄テキストはどの dry-run say にも現れない（wire 非搭載）。
    assert!(
        captured(&buf)
            .iter()
            .all(|c| !c.body.contains("NRTERM-GARBAGE")),
        "破棄テキストが wire(dry-run say) に現れた: {:?}",
        captured(&buf)
    );

    // (ii) 破棄ログが 1 件出ており、破棄全文と session_id を持つ（A1L）。
    let ok_discard = {
        let dbuf = dbuf.clone();
        wait_until(move || {
            discards(&dbuf)
                .iter()
                .any(|d| d.discarded.contains("NRTERM-GARBAGE"))
        })
        .await
    };
    assert!(
        ok_discard,
        "破棄ログ(no_reply_trailing_discarded) が出ていない: {:?}",
        discards(&dbuf)
    );
    let d = discards(&dbuf)
        .into_iter()
        .find(|d| d.discarded.contains("NRTERM-GARBAGE"))
        .unwrap();
    assert!(
        d.discarded.contains("NO_REPLY"),
        "破棄全文に NO_REPLY トークンが含まれない: {:?}",
        d
    );
    assert!(
        !d.session_id.is_empty(),
        "破棄ログに session_id 相関キーが無い: {:?}",
        d
    );
}

// ==================== (c) 同一イベントが両車線 → said は 1 回だけ ====================

#[tokio::test]
async fn scenario_c_same_event_on_both_lanes_says_once() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text("QCC-ACK ひとつだけ返すよ");
    // 万一 2 ターン走ったら 2 本目が消費される。後段で system_prompts 数を見て 1 本を確かめる。
    mock.push_text("QCC-EXTRA 余計な二本目");
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // default(mention) 車線 + watch 車線。fake_watch は全車線へ同じ fixture を流すので、
    // 同一行が両車線に届く。自分宛て #p メンションは watch 車線が default 車線へ譲る
    // （Defect A / QC #10）。ネットで said は 1 回・ターンは 1 本・say は 1 通。
    let watches = serde_json::json!([{
        "id": 7,
        "interval_secs": 3600,
        "filter_json": { "authors": [author_pk()] }
    }]);
    let fixture = Fixture::new();
    let (_client, _address, _session_id) =
        wire_instance(&core, &fixture, nostr_config(Some(watches))).await;

    let event_id = "c1".repeat(32);
    fixture.append_line(&mention_event(&event_id, "QCC-MARK 両車線に届く本文"));

    // say が出るまで待つ。
    let ok = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, "QCC-ACK").is_some()).await
    };
    assert!(ok, "say が出ない: {:?}", captured(&buf));

    // 2 本目が来ないことを確かめるための落ち着き時間（interval=3600s なので flush は無い）。
    tokio::time::sleep(Duration::from_millis(400)).await;

    let n_says = captured(&buf)
        .iter()
        .filter(|c| c.body.contains("QCC-ACK"))
        .count();
    assert_eq!(n_says, 1, "同一イベントで say が複数出た: {n_says}");
    assert_eq!(
        mock.system_prompts().len(),
        1,
        "ターンが 1 本でない（両車線で二重に走った）"
    );
}

// ==================== (main) 長い処理中の第2依頼: 3 say が順に出る ====================

// ルーティング用マーカー（会話へ現れる substring・互いに部分文字列にならないよう分離）。
const M_FIRST: &str = "MARKER-ONE";
const M_SECOND: &str = "MARKER-TWO";
const M_SUBTASK: &str = "MARKER-SUB";
// 応答本文（say として観測する。マーカーとも互いとも部分一致しない）。
const B_ACK: &str = "ackbody-alpha 了解、長い処理を始めたよ";
const B_SECOND: &str = "secondbody-beta 回答は 4 だよ";
const B_COMPLETION: &str = "completionbody-gamma 長い処理おわったよ";
const B_SUBTASK_RESULT: &str = "subresult-delta 内部結果";

/// 内容ルーティング mock。指定ターンだけ `Notify` で待たせる（並行ターンが FIFO を奪い合わない）。
struct RoutedMock {
    released: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl RoutedMock {
    fn new() -> Self {
        Self {
            released: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[async_trait::async_trait]
impl LlmProvider for RoutedMock {
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
        let text = request_text(&request);

        // (E) subtask 決着後の resume ターン → 完了報告 say(3)。
        // 注意: システムプロンプトにはツール解説として "subtask_completed" が常に含まれるため、
        // それでは判定できない。決着で親ログに載る subtask 結果本文（会話に現れる）で判定する。
        if text.contains(B_SUBTASK_RESULT) {
            return Ok(text_response(B_COMPLETION));
        }
        // (B) 親ターン#1 の spawn_subtask 実行後（tool_result 有り）→ 即応 ack say(1)。
        if has_tool_role(&request) {
            return Ok(text_response(B_ACK));
        }
        // (D) 第2依頼のターン → 即応 say(2)。
        if text.contains(M_SECOND) {
            return Ok(text_response(B_SECOND));
        }
        // (A) 親ターン#1 の初回 → spawn_subtask を呼んで背景サブタスクを detach。
        if text.contains(M_FIRST) {
            return Ok(tool_call_response(
                "spawn_subtask",
                serde_json::json!({
                    "task": format!("{M_SUBTASK} 長い処理を実行して"),
                    "timeout_secs": 120,
                }),
            ));
        }
        // (C) 背景サブタスクの sub-run → テストが release するまでブロック（= 長い処理の走行中）。
        if text.contains(M_SUBTASK) {
            loop {
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                let waiter = self.notify.notified();
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                waiter.await;
            }
            return Ok(text_response(B_SUBTASK_RESULT));
        }
        Err(anyhow::anyhow!("RoutedMock: unrouted request: {text}"))
    }
}

#[tokio::test]
async fn scenario_main_second_request_not_blocked_during_long_op() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev1 = "b1".repeat(32);
    let ev2 = "b2".repeat(32);

    // 1) 「長い処理して終わったら教えて」→ 即応 ack say(1) ＋ 背景サブタスク detach。
    fixture.append_line(&mention_event(
        &ev1,
        &format!("{M_FIRST} 長い処理して終わったら教えて"),
    ));

    // say(1) が出る AND 背景サブタスクが走行中（held）になるまで待つ。
    let ack_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_ACK).is_some()).await
    };
    assert!(ack_ready, "ack say(1) が出ない: {:?}", captured(&buf));
    let running = {
        let state = core.state.clone();
        let sid = session_id.clone();
        wait_until(move || state.subtask_registries.has_running(&sid)).await
    };
    assert!(
        running,
        "背景サブタスクが走行中にならない（detach/hold 失敗）"
    );

    // 2) 走行中に第2依頼を投入 → ブロックされず即応 say(2)。
    fixture.append_line(&mention_event(&ev2, &format!("{M_SECOND} 2 足す 2 は?")));
    let second_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_SECOND).is_some()).await
    };
    assert!(
        second_ready,
        "第2依頼が長い処理にブロックされている（say(2) 未達）: {:?}",
        captured(&buf)
    );
    // この時点でサブタスクはまだ走行中（held）のはず。
    assert!(
        core.state.subtask_registries.has_running(&session_id),
        "第2依頼処理中にサブタスクが既に終わっている（hold が効いていない）"
    );

    // 3) サブタスクを解放 → 決着 → resume → 完了報告 say(3)。
    mock.release();
    let completion_ready = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_COMPLETION).is_some()).await
    };
    assert!(
        completion_ready,
        "完了報告 say(3) が出ない: {:?}",
        captured(&buf)
    );

    // 3 say が 1→2→3 の順で並ぶ（朝のバグ=第2依頼ブロックが無いことの再現）。
    let i1 = body_index(&buf, B_ACK).expect("ack");
    let i2 = body_index(&buf, B_SECOND).expect("second");
    let i3 = body_index(&buf, B_COMPLETION).expect("completion");
    assert!(
        i1 < i2 && i2 < i3,
        "say の順序が 1→2→3 でない: ack={i1} second={i2} completion={i3} / {:?}",
        captured(&buf)
    );
}

// ==================== (shell) execute_shell の stdout が resume 会話に現れる ====================
//
// くらぶ暴走の根因回帰。`execute_shell` は inline 集合に無いため常に背景 subtask 化され
// （#152/#671）、その完了本文（＝ツール結果 JSON）は会話再構成で参照へ畳まれていた（#713）。
// #713 の「同ターン内は本文がモデルに渡る」前提は inline ツールでのみ成立し、execute_shell は
// 同ターン往復が無いので、畳むと stdout をどのターンでも読めず、モデルが出力を取り直そうと
// 待機宣言を連投した（実機で確認）。修正で exit_code を持つ結果は畳まず stdout を会話へ残す。
//
// このハーネスは実配線（実 dispatch 判定 → 実 execute_shell = 実 echo → 実 settle_completed →
// 実 resume）を通す。ピン: **resume ターンの会話（LLM リクエスト本文）に echo の stdout が現れる**
// ——修正前はここで落ちる（参照へ畳まれ stdout が消える）。

const M_SHELL: &str = "SHELLQC-MARK 東京の天気を調べて教えて";
/// echo で実際に出力させる stdout。マーカーとも ack/done 本文とも部分一致しない。
const SHELL_STDOUT: &str = "SHELLOUT-tenki 晴れ 28度 くもり所により雨";
const B_SHELL_ACK: &str = "shellack-epsilon 調べてるよ、ちょっと待ってね";
const B_SHELL_DONE: &str = "shelldone-zeta 東京は晴れ 28度だよ";

/// execute_shell の段階応答 mock。全リクエスト本文を記録し、resume ターンの本文を surface する。
struct ShellMock {
    shell_emitted: AtomicBool,
    shell_calls: AtomicUsize,
    /// resume（決着後の再開ターン）で会話に渡された本文。ピンの検証対象。
    resume_text: Mutex<Option<String>>,
}

impl ShellMock {
    fn new() -> Self {
        Self {
            shell_emitted: AtomicBool::new(false),
            shell_calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ShellMock {
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
        let text = request_text(&request);

        // (2) dispatch 直後の継続イテレーション（合成 "spawned" 結果 = tool role）→ ack say で
        //     ターンを閉じる。ここで背景 subtask（echo）が走る。
        if has_tool_role(&request) {
            return Ok(text_response(B_SHELL_ACK));
        }
        // (1) 初回メンション（tool role 無し・最初の 1 回だけ）→ execute_shell を呼ぶ。
        //     opencrab は execute_shell を inline 化しないので背景 subtask へ回る。
        if !self.shell_emitted.swap(true, Ordering::SeqCst) {
            self.shell_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [SHELL_STDOUT] }),
            ));
        }
        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 会話本文を捕まえて
        //     完了報告 say で閉じる。**この text に echo の stdout が含まれていること**がピン。
        *self.resume_text.lock().unwrap() = Some(text);
        Ok(text_response(B_SHELL_DONE))
    }
}

/// echo だけを許可した shell 有効の tools 設定。
fn shell_enabled_tools_config() -> opencrab_actions::tools::ToolsConfig {
    opencrab_actions::tools::ToolsConfig {
        enabled: true,
        shell: Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["echo".to_string()],
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn scenario_shell_stdout_survives_into_resume_turn() {
    let buf = install_capture();
    let mock = Arc::new(ShellMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // execute_shell を実際に走らせるため shell を有効化（echo のみ許可）。tools_config は
    // Arc<RwLock> 共有なので serve_uds へ渡った runtime にも即時反映される。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "d1".repeat(32);
    fixture.append_line(&mention_event(&ev, M_SHELL));

    // 決着後の完了報告 say が出るまで待つ（= resume ターンまで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_SHELL_DONE).is_some()).await
    };
    assert!(
        done,
        "決着後の resume 完了報告が出ない（execute_shell の背景 subtask が resume まで到達しない）: {:?}",
        captured(&buf)
    );

    // ピン: resume ターンの会話本文に echo の stdout が現れる（修正前はここで落ちる）。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains(SHELL_STDOUT),
        "resume 会話に execute_shell の stdout が無い（畳まれた＝くらぶ暴走の根因）。\n\
         resume 本文（先頭 2000 字）: {:.2000}",
        resume_text
    );

    // 行動系: execute_shell の dispatch はちょうど 1 回（再取得＝取り直しループをしていない）。
    assert_eq!(
        mock.shell_calls.load(Ordering::SeqCst),
        1,
        "execute_shell が複数回 dispatch された（取り直しループ）"
    );
    // ack say（「待ってね」相当）は高々 1 回（待機宣言を連投しない）。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_SHELL_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

// ========== (#880) exit_code 無しの dispatch ツールの結果本文が resume 会話に現れる ==========
//
// #877 の E2E（execute_shell の stdout が resume 会話に残る）を **exit_code 無しの dispatch ツール**
// へ拡張した回帰（設計 §6 A2 の第二ケース）。`ws_write` は `CORE_DISPATCHABLE_ACTIONS` にあり
// 常に背景 subtask 化される・戻り値に `exit_code` を持たない。#877 は exit_code を持つ結果だけ
// 畳みを撤回したので、ws_write のような exit_code 無し dispatch ツールの結果本文は「結果 N 文字」へ
// 畳まれ、切り離した subtask の結果を resume がどのターンでも読めず再 dispatch の燃料になっていた
// （症状B）。#880 で exit_code の有無に関わらず本文（payload）を会話へ残す。
//
// このハーネスは実配線（実 dispatch 判定 → 実 ws_write → 実 settle_completed → 実 resume）を通す。
// ピン: **resume ターンの会話本文に ws_write の payload（書いた path）が現れる**——修正前はここで
// 落ちる（「結果 N 文字」へ畳まれ path が消える）。加えて再 dispatch 無し（有界）を固定する。

/// mock agent が ws_write に渡す（＝結果 payload に現れる）path。ack/done 本文と部分一致しない。
const WS_WRITE_PATH: &str = "wsqc-880-notes.md";
const M_WSWRITE: &str = "WSWRITEQC-MARK 設計メモを保存して";
const B_WSWRITE_ACK: &str = "wswriteack-eta 保存するね、ちょっと待ってて";
const B_WSWRITE_DONE: &str = "wswritedone-theta 保存したよ";

/// ws_write の段階応答 mock。resume ターンの会話本文を surface する。
struct WsWriteMock {
    emitted: AtomicBool,
    calls: AtomicUsize,
    resume_text: Mutex<Option<String>>,
}

impl WsWriteMock {
    fn new() -> Self {
        Self {
            emitted: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for WsWriteMock {
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
        let text = request_text(&request);
        // (2) dispatch 直後の継続イテレーション（合成 "spawned" 結果 = tool role）→ ack say で閉じる。
        if has_tool_role(&request) {
            return Ok(text_response(B_WSWRITE_ACK));
        }
        // (1) 初回メンション → ws_write を呼ぶ（inline 化されないので背景 subtask へ回る）。
        if !self.emitted.swap(true, Ordering::SeqCst) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "ws_write",
                serde_json::json!({ "path": WS_WRITE_PATH, "content": "# 設計メモ\n本文" }),
            ));
        }
        // (3) 決着後の resume ターン → 会話本文を捕まえて完了報告 say で閉じる。
        //     **この text に ws_write の path（payload）が含まれていること**がピン。
        *self.resume_text.lock().unwrap() = Some(text);
        Ok(text_response(B_WSWRITE_DONE))
    }
}

#[tokio::test]
async fn scenario_no_exit_code_dispatch_result_survives_into_resume_turn() {
    let buf = install_capture();
    let mock = Arc::new(WsWriteMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e1".repeat(32);
    fixture.append_line(&mention_event(&ev, M_WSWRITE));

    // 決着後の完了報告 say が出るまで待つ（= resume ターンまで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_WSWRITE_DONE).is_some()).await
    };
    assert!(
        done,
        "決着後の resume 完了報告が出ない（ws_write の背景 subtask が resume まで到達しない）: {:?}",
        captured(&buf)
    );

    // ピン: resume ターンの会話本文に ws_write の payload（書いた path）が現れる（修正前は畳まれて消える）。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains(WS_WRITE_PATH),
        "resume 会話に exit_code 無し dispatch ツール（ws_write）の結果本文が無い（畳まれた＝症状B の燃料）。\n\
         resume 本文（先頭 2000 字）: {:.2000}",
        resume_text
    );

    // 行動系: ws_write の dispatch はちょうど 1 回（結果を読めるので取り直しループをしない）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        1,
        "ws_write が複数回 dispatch された（取り直しループ）"
    );
    // ack say（待機宣言）は高々 1 回（連投しない）。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_WSWRITE_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

// ============ (shell 大出力) offload → ws_read 読み戻し → 回答（再帰ループ閉包 E2E） ============
//
// #856 発見3 の回帰。#877 の E2E（小出力が resume 会話に verbatim で残る）を **大出力版**へ拡張し、
// 「大結果を畳む→レシピで読み戻す→その読み出しがまた畳まれて読めないループ」が閉じていることを
// 実配線で固定する:
//
//   execute_shell が大出力（>2,500 tok）を返す → 会話へ届く前に workspace/tmp へ offload され
//   回収レシピ付き notice に化ける（#551）→ resume ターンで mock agent が notice の tmp パスを
//   **実 ws_read**（inline）で読み戻す → 読めた本文で回答し、**同じ execute_shell を再実行しない**。
//
// ピン: (a) resume 会話に offload notice（tmp パス）が現れる、(b) mock が ws_read で読んだ本文に
// 大出力のマーカーが含まれる、(c) execute_shell の dispatch はちょうど 1 回・ws_read も 1 回
// （読み戻しが再 offload → 再 ws_read の無限ループになっていない）、(d) ack say は高々 1 回。

/// resume 会話に載る offload notice のマーカー（大出力の先頭行）。ws_read で読み戻すと
/// ws_read 結果の tool メッセージにこの文字列が現れる＝実際に読めている証拠。
const BIG_OUT_MARK: &str = "BIGOUT-marker 東京の天気 晴れ 28度";
const M_BIG_SHELL: &str = "BIGSHELLQC-MARK 大きな出力のコマンドを実行して結果を教えて";
const B_BIG_ACK: &str = "bigack-eta 実行中、ちょっと待ってね";
const B_BIG_DONE: &str = "bigdone-theta 読み終わった、東京は晴れ 28度だよ";

/// echo に渡す大出力（>2,500 tok）。実改行を含む単一 arg なので echo がそのまま複数行で吐く。
/// 先頭行に [`BIG_OUT_MARK`]。offload 閾値を確実に超える大きさにする。
fn big_shell_payload() -> String {
    let body =
        "src/foo.rs:99:    let value = compute(argument, more, and_more); // dense output row";
    std::iter::once(BIG_OUT_MARK.to_string())
        .chain(std::iter::repeat_n(body.to_string(), 400))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tool ロールのメッセージ本文を連結（合成 spawned 結果か ws_read 結果本文かの判別に使う）。
fn tool_role_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 大出力 shell の段階応答 mock。offload notice を読み、レシピどおり ws_read で読み戻す。
struct BigShellMock {
    shell_emitted: AtomicBool,
    ws_read_emitted: AtomicBool,
    shell_calls: AtomicUsize,
    ws_read_calls: AtomicUsize,
    /// resume ターンで会話に渡された本文（offload notice を含むはず）。
    resume_text: Mutex<Option<String>>,
    /// ws_read が返した本文（マーカーを含むはず＝実際に読めた証拠）。
    read_back_text: Mutex<Option<String>>,
}

impl BigShellMock {
    fn new() -> Self {
        Self {
            shell_emitted: AtomicBool::new(false),
            ws_read_emitted: AtomicBool::new(false),
            shell_calls: AtomicUsize::new(0),
            ws_read_calls: AtomicUsize::new(0),
            resume_text: Mutex::new(None),
            read_back_text: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for BigShellMock {
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
        let text = request_text(&request);

        if has_tool_role(&request) {
            let tool_text = tool_role_text(&request);
            // ws_read の結果（マーカー入り本文）が返ってきた → 読めた本文で回答。
            // **同じ execute_shell は再実行しない**（回答して閉じる）。
            if tool_text.contains(BIG_OUT_MARK) {
                *self.read_back_text.lock().unwrap() = Some(tool_text);
                return Ok(text_response(B_BIG_DONE));
            }
            // それ以外（dispatch 直後の合成 `spawned` 結果）→ ack でターンを閉じる。
            return Ok(text_response(B_BIG_ACK));
        }

        // (1) 初回メンション（tool role 無し・最初の 1 回）→ 大出力 execute_shell を呼ぶ。
        if !self.shell_emitted.swap(true, Ordering::SeqCst) {
            self.shell_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [big_shell_payload()] }),
            ));
        }

        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目）→ 会話の offload notice から
        //     tmp パスを取り出し、**レシピどおり ws_read で読み戻す**（inline 実行）。
        if !self.ws_read_emitted.swap(true, Ordering::SeqCst) {
            *self.resume_text.lock().unwrap() = Some(text.clone());
            // notice の単独 rel トークン（`tmp/…txt`）を拾う。回収レシピの複合トークン
            // （`grep -n <pattern> tmp/…` 等）ではなく、backtick で囲われた素のパスを取る。
            let rel = text
                .split('`')
                .find(|t| t.starts_with("tmp/") && t.ends_with(".txt"))
                .unwrap_or("tmp/MISSING.txt")
                .to_string();
            self.ws_read_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_response(
                "ws_read",
                serde_json::json!({ "path": rel, "start_line": 1 }),
            ));
        }

        // フォールバック（想定外の追加ターン）→ 回答で閉じる。
        Ok(text_response(B_BIG_DONE))
    }
}

#[tokio::test]
async fn scenario_shell_big_output_offload_read_back_loop_closed() {
    let buf = install_capture();
    let mock = Arc::new(BigShellMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // echo のみ許可（大出力 arg を吐かせる）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e2".repeat(32);
    fixture.append_line(&mention_event(&ev, M_BIG_SHELL));

    // 読み戻し後の完了報告 say が出るまで待つ（= offload→ws_read→回答まで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, B_BIG_DONE).is_some()).await
    };
    assert!(
        done,
        "読み戻し後の完了報告が出ない（大出力の offload→ws_read→回答チェーンが閉じない）: {:?}",
        captured(&buf)
    );

    // (a) resume 会話に offload notice（tmp パス）が現れる。
    let resume_text = mock
        .resume_text
        .lock()
        .unwrap()
        .clone()
        .expect("resume ターンが実行されていない");
    assert!(
        resume_text.contains("Tool result withheld"),
        "resume 会話に offload notice が無い（大出力が畳まれていない）:\n{:.600}",
        resume_text
    );
    assert!(
        resume_text.contains("tmp/") && resume_text.contains("ws_read"),
        "notice に読める handle（tmp パス＋ws_read レシピ）が無い:\n{:.600}",
        resume_text
    );

    // (b) mock が ws_read で読み戻した本文に大出力のマーカーがある（実際に読めている）。
    let read_back = mock
        .read_back_text
        .lock()
        .unwrap()
        .clone()
        .expect("ws_read の結果ターンが実行されていない");
    assert!(
        read_back.contains(BIG_OUT_MARK),
        "ws_read で読み戻した本文にマーカーが無い（回収レシピが機能していない）:\n{:.600}",
        read_back
    );
    // 読み戻した ws_read 結果自体は再 offload されていない（notice ではなく実本文が来ている）。
    assert!(
        !read_back.contains("Tool result withheld"),
        "ws_read 結果がまた offload された＝読み戻しがループする（#856 発見3 が閉じていない）:\n{:.600}",
        read_back
    );

    // (c) execute_shell の dispatch はちょうど 1 回・ws_read もちょうど 1 回
    //     （読み戻しが再 offload→再 ws_read の無限ループになっていない）。
    assert_eq!(
        mock.shell_calls.load(Ordering::SeqCst),
        1,
        "execute_shell が複数回 dispatch された（取り直しループ）"
    );
    assert_eq!(
        mock.ws_read_calls.load(Ordering::SeqCst),
        1,
        "ws_read が複数回走った（読み戻しが再 offload→再 ws_read のループに入った）"
    );

    // (d) ack say（待機宣言）は高々 1 回。
    let acks = captured(&buf)
        .iter()
        .filter(|c| c.body.contains(B_BIG_ACK))
        .count();
    assert!(acks <= 1, "ack say（待機宣言）が連投された: {acks} 回");
}

// ==================== (A3) 発話クラス reply: 撃ちっぱなし（第三柱・§3.3.1） ====================
//
// DESIGN-RESUME-SETTLE §6 A3: reply/reaction は (i) subtask/settle/resume を起こさない・
// (ii) 会話ログに機械行（tool_call/tool_result/sN）を残さない（本文＋関係注記のみ）・
// (iii) 配送される（dry-run に出る）。実配線（実 extgate + 実 nostr-gateway dry-run invoke）を通す。

const M_A3: &str = "A3REPLY-MARK";
const B_A3: &str = "A3-返信本文だよ";
const M_A3_THREE: &str = "A3-THREE-REPLIES-MARK";
const B_A3_THREE: [&str; 3] = ["A3-返信その1", "A3-返信その2", "A3-返信その3"];

struct A3Mock {
    chat_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for A3Mock {
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
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        let text = request_text(&request);
        // 発話 reply の最小 ack（tool role）を受けたら沈黙で閉じる（resume は起きない）。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        if text.contains(M_A3_THREE) {
            return Ok(tool_calls_response(
                B_A3_THREE
                    .iter()
                    .map(|body| ("reply", serde_json::json!({"event": "e1", "text": body})))
                    .collect(),
            ));
        }
        if text.contains(M_A3) {
            return Ok(tool_call_response(
                "reply",
                serde_json::json!({"event": "e1", "text": B_A3}),
            ));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn scenario_a3_reply_utterance_no_subtask_no_machine_lines() {
    let buf = install_capture();
    let mock = Arc::new(A3Mock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a3".repeat(32);
    fixture.append_line(&mention_event(&ev, &format!("{M_A3} これに返信して")));

    // (iii) reply が配送される（dry-run に kind="reply" body が出る）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_A3))
        })
        .await
    };
    assert!(delivered, "発話 reply が配送されない: {:?}", captured(&buf));

    // (i) subtask/settle/resume が起きない: reply は inline 発話なので背景 subtask に載らない。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "発話 reply が subtask 化された（撃ちっぱなしでない）"
    );
    // reply は 1 回だけ（resume 復唱・自動再送ゼロ）。
    let reply_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.body.contains(B_A3))
        .count();
    assert_eq!(
        reply_count,
        1,
        "reply が複数回配送された（復唱）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        1,
        "発話 reply は最小 ack 往復を起こさず 1 生成で完了する"
    );

    // (ii) 機械行を残さない: session_logs に reply の tool_call/tool_result が無く、本文は speech で残る。
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    let kinds: Vec<(&str, &str)> = logs
        .iter()
        .map(|l| (l.log_type.as_str(), l.content.as_str()))
        .collect();
    assert!(
        !logs
            .iter()
            .any(|l| l.log_type == "tool_call" || l.log_type == "tool_result"),
        "発話 reply が機械行(tool_call/tool_result)を残した: {kinds:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.log_type == "speech" && l.content.contains(B_A3)),
        "reply 本文が speech として残っていない: {kinds:?}"
    );
}

/// #880: reply×3 を 1 生成に並べ、3 通を配送して LLM 往復なしで完了する。
#[tokio::test]
async fn scenario_a3_three_replies_complete_in_one_llm_call_without_subtask() {
    let buf = install_capture();
    let mock = Arc::new(A3Mock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a4".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        &format!("{M_A3_THREE} 3回に分けて返信して"),
    ));

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_A3_THREE.iter().all(|body| {
                captured(&buf)
                    .iter()
                    .any(|captured| captured.kind == "reply" && captured.body.contains(body))
            })
        })
        .await
    };
    assert!(
        delivered,
        "1 生成の reply×3 がすべて配送されない: {:?}",
        captured(&buf)
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    for body in B_A3_THREE {
        let count = captured(&buf)
            .iter()
            .filter(|captured| captured.kind == "reply" && captured.body.contains(body))
            .count();
        assert_eq!(count, 1, "reply 本文 {body} の配送回数が 1 でない");
    }
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        1,
        "reply×3 は ack ごとの LLM 再呼び出しを起こさない"
    );
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "reply×3 が subtask 化された"
    );
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        !logs
            .iter()
            .any(|log| log.log_type == "tool_call" || log.log_type == "tool_result"),
        "reply×3 が機械行を残した: {:?}",
        logs.iter()
            .map(|log| (&log.log_type, &log.content))
            .collect::<Vec<_>>()
    );
    // §13 ターン合計 reply3-in-one: 保存 3（memory_sessions の agent 発話 speech 行が 3）。
    // speaker_id==AGENT_ID で発端メッセージ（inbound・speaker=送信者）を除いて数える。
    let agent_speech_saves = logs
        .iter()
        .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
        .count();
    assert_eq!(
        agent_speech_saves,
        3,
        "reply×3 in one の agent 発話 speech 保存が 3 でない（§13 reply3-in-one=保存3）: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.speaker_id, &l.content))
            .collect::<Vec<_>>()
    );
}

// ==================== (A3-CONTINUE) 発話のみ＋末尾 CONTINUE で継続（#900） ====================
//
// #900: reply（発話クラス）のみの生成でも content 末尾が CONTINUE 単独なら、発話を配送してから
// 次イテレーションへ進む。reply×1＋CONTINUE を 3 回連ねると、3 通配送・LLM 3 呼び出し・CONTINUE は
// 本文へ残らない（撃ちっぱなし＋末尾マーカーの併記契約）。旧挙動（純発話は 1 生成で必ず完結）だと
// 1 通・LLM 1 で止まるので、この差が回帰ガードになる。

// マーカー文字列自体に "CONTINUE" を含めない（残留検査で発端メッセージが誤検知するのを避ける）。
const M_A3_CONT: &str = "A3CONTMARK";
const B_A3_CONT: [&str; 3] = ["A3-CONT返信1", "A3-CONT返信2", "A3-CONT返信3"];

/// reply tool_call と content を同一生成に載せる（tool_calls_response は content=None のため）。
fn reply_with_content_response(text: &str, content: &str) -> ChatResponse {
    let mut resp = tool_call_response("reply", serde_json::json!({"event": "e1", "text": text}));
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

struct A3ContinueMock {
    chat_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for A3ContinueMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        // 生成回数で分岐する（純発話＋CONTINUE の継続は tool role ack を返すため、text マーカーだけ
        // では 1・2・3 回目を区別できない）。1・2 回目は reply＋末尾 CONTINUE、3 回目は reply のみ。
        let n = self.chat_calls.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            Ok(reply_with_content_response(B_A3_CONT[n], "CONTINUE"))
        } else {
            Ok(tool_call_response(
                "reply",
                serde_json::json!({"event": "e1", "text": B_A3_CONT[2]}),
            ))
        }
    }
}

/// #900: reply×1＋末尾 CONTINUE を 3 回連ねる → 3 通配送・LLM 3 呼び出し・CONTINUE 非残留。
#[tokio::test]
async fn scenario_a3_utterance_only_with_continue_runs_next_iteration() {
    let buf = install_capture();
    let mock = Arc::new(A3ContinueMock {
        chat_calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "a5".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        &format!("{M_A3_CONT} 3回に分けて返信して"),
    ));

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_A3_CONT.iter().all(|body| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(body))
            })
        })
        .await
    };
    assert!(
        delivered,
        "reply×1＋CONTINUE の 3 連が全通配送されない（継続が起きていない）: {:?}",
        captured(&buf)
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    for body in B_A3_CONT {
        let count = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(body))
            .count();
        assert_eq!(count, 1, "reply 本文 {body} の配送回数が 1 でない");
    }
    // 3 回の生成すべてが走る（1・2 回目は CONTINUE で継続、3 回目で自然終了）。
    assert_eq!(
        mock.chat_calls.load(Ordering::SeqCst),
        3,
        "純発話＋末尾 CONTINUE が次イテレーションを起こさない（1 生成で止まった）"
    );
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "発話＋CONTINUE が subtask 化された"
    );
    // CONTINUE は say としても speech ログとしても残らない（剥がされて空になる）。
    let no_continue_captured = captured(&buf).iter().all(|c| !c.body.contains("CONTINUE"));
    assert!(
        no_continue_captured,
        "配送本文に CONTINUE が残留: {:?}",
        captured(&buf)
    );
    let logs = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        !logs.iter().any(|l| l.content.contains("CONTINUE")),
        "session_logs に CONTINUE が残留: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.content))
            .collect::<Vec<_>>()
    );
    // §13 ターン合計 reply1＋CONTINUE×2: 保存 3（各イテレーションの reply が speech 保存される）。
    let agent_speech_saves = logs
        .iter()
        .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
        .count();
    assert_eq!(
        agent_speech_saves, 3,
        "reply1＋CONTINUE×2 の agent 発話 speech 保存が 3 でない（§13=保存3・§12.2 各イテレーション保存）: {:?}",
        logs.iter()
            .map(|l| (&l.log_type, &l.speaker_id, &l.content))
            .collect::<Vec<_>>()
    );
}

// ==================== (N2) 照会クラス resolve: 従来どおり subtask 化（非回帰・§6 N2） ====================
//
// resolve は結果を読む照会クラス。発話クラス化に巻き込まれず、従来どおり Dispatchable →
// 背景 subtask → 機械行（tool_call）を残す。A3（発話）との構造対比で「照会は殺していない」を固定。
// settle→resume が結果を読む経路自体は scenario_main_second_request_not_blocked_during_long_op
// （spawn_subtask の settle→resume→完了 say）が別途固定している。

const M_N2: &str = "N2RESOLVE-MARK";

struct N2Mock;

#[async_trait::async_trait]
impl LlmProvider for N2Mock {
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
        let text = request_text(&request);
        if has_tool_role(&request) {
            // spawned ack / resume は沈黙で閉じる（本テストは分類=照会の構造だけを見る）。
            return Ok(text_response("NO_REPLY"));
        }
        if text.contains(M_N2) {
            return Ok(tool_call_response(
                "resolve",
                serde_json::json!({"ref": "e1"}),
            ));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn scenario_n2_resolve_query_class_keeps_subtask_and_machine_line() {
    let buf = install_capture();
    let mock = Arc::new(N2Mock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "c2".repeat(32);
    fixture.append_line(&mention_event(&ev, &format!("{M_N2} これの全文を見て")));

    // 照会 resolve は Dispatchable → tool_call 機械行が残る（発話クラスと異なり撃ちっぱなしでない）。
    let saw_resolve_call = {
        let core = &core;
        let session_id = session_id.clone();
        wait_until(move || {
            let conn = core.extgate.db.lock().unwrap();
            let logs =
                opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap();
            // resolve の名は tool_call ログの metadata（tool_calls_json）に載る（content は空）。
            logs.iter().any(|l| {
                l.log_type == "tool_call"
                    && l.metadata_json
                        .as_deref()
                        .is_some_and(|m| m.contains("resolve"))
            })
        })
        .await
    };
    // resolve が発話クラスに誤分類されていれば invoke_utterance 経由になり tool_call 機械行を
    // 残さない。tool_call ログの存在が「照会クラス（Dispatchable・subtask）のまま」を証す。
    // （dry-run 配送 buffer は全テスト共有でここでは判定に使わない。）
    assert!(
        saw_resolve_call,
        "resolve の tool_call 機械行が残らない（照会が発話クラスに誤分類された）"
    );
    let _ = &buf;
}

// ==================== (#898) CONTINUE 途中イテレーションの発話配送・保存 ====================
//
// DESIGN-TURN-CONTINUATION §11.1: 末尾 CONTINUE の生成は「残りの content を通常どおり配送・
// 保存 → 次イテレーション」。reply を使わない純テキストを 3 分割（1回目 CONTINUE / 2回目
// CONTINUE / 3回目）した場合、途中イテレーション（1回目・2回目）の発話も say として配送され、
// memory_sessions に speech として保存されること。#895 は engine 側の on_response_text 発火を
// モックで固定したが extgate V3 の実配線（apply_delivery_effect は最終 EngineResult.response
// のみ配送）を通しておらず、途中発話が配送も保存もされずに落ちていた（#898 QC 実弾で確認）。

/// マーカー（テスト間で共有される dry-run buffer / DB を絞る）。
const C898_1: &str = "C898-1回目。まず一つ";
const C898_2: &str = "C898-2回目。次いこう";
const C898_3: &str = "C898-3回目。これで最後";

#[tokio::test]
async fn scenario_continue_intermediate_speech_delivered_and_saved() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    // reply を使わない純テキスト 3 分割。末尾 CONTINUE で継続、3 回目は継続せず終了。
    mock.push_text(&format!("{C898_1}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{C898_2}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{C898_3}\u{26a1}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "d8".repeat(32);
    fixture.append_line(&mention_event(&ev, "3回に分けて投稿して reply使わずに"));

    // (i) 3 分割の全 say が standalone post として配送される（途中発話 1回目/2回目も届く）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            let says = captured(&buf);
            let has = |needle: &str| {
                says.iter()
                    .any(|c| c.kind == "standalone" && c.body.contains(needle))
            };
            has(C898_1) && has(C898_2) && has(C898_3)
        })
        .await
    };
    assert!(
        delivered,
        "CONTINUE 途中イテレーションの発話が say 配送されない: {:?}",
        captured(&buf)
    );

    // (ii) 配送された say の本文に CONTINUE マーカーが残らない（§11.6）。
    let c898_says: Vec<CapturedSay> = captured(&buf)
        .into_iter()
        .filter(|c| c.body.contains("C898-"))
        .collect();
    assert!(
        c898_says.iter().all(|c| !c.body.contains("CONTINUE")),
        "say 本文に CONTINUE が残留した: {c898_says:?}"
    );

    // (iii) LLM は 3 回呼ばれる（末尾 CONTINUE が 2 回の追加イテレーションを起こす）。
    assert_eq!(
        mock.system_prompts().len(),
        3,
        "末尾 CONTINUE で LLM が 3 回呼ばれていない"
    );

    // (iv) memory_sessions に 3 件の speech が保存され、いずれにも CONTINUE が残らない。
    let speeches: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech" && l.content.contains("C898-"))
            .map(|l| l.content)
            .collect()
    };
    assert_eq!(
        speeches.len(),
        3,
        "途中イテレーションの発話が memory_sessions に保存されていない: {speeches:?}"
    );
    assert!(
        speeches.iter().all(|s| !s.contains("CONTINUE")),
        "保存された speech に CONTINUE が残留した: {speeches:?}"
    );
}

// ==================== (#898 §13.1 j) 途中配送失敗で継続を止める ====================
//
// DESIGN-TURN-CONTINUATION.md §13.1 j「途中イテレーションの投稿が配送失敗（ゲート error）→
// 既存の発話失敗経路（❌/turn_failed）・継続は止める（失敗を隠して次に進まない）」。
// 観測境界: LLM 呼び出し回数（継続が止まれば 1 回だけ・2/3 回目のイテレーションへ進まない）。

const J898_1: &str = "J898-1回目。まず一つ";
const J898_2: &str = "J898-2回目。次いこう";
const J898_3: &str = "J898-3回目。これで最後";

#[tokio::test]
async fn scenario_continue_intermediate_delivery_failure_stops_continuation() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    mock.push_text(&format!("{J898_1}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{J898_2}\u{26a1}\nCONTINUE"));
    mock.push_text(&format!("{J898_3}\u{26a1}"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    // 途中発話（say）の配送をゲート error（disconnect）にする。
    core.extgate
        .probe
        .fail_say_write
        .store(true, Ordering::SeqCst);

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "e9".repeat(32);
    fixture.append_line(&mention_event(&ev, "3回に分けて・途中で配送失敗"));

    // 少なくとも 1 回目のイテレーションは走る。
    let started = {
        let mock = mock.clone();
        wait_until(move || !mock.system_prompts().is_empty()).await
    };
    assert!(started, "最初のイテレーションすら走っていない");

    // 追加イテレーションが走らないことを確認する猶予（走ってしまうなら継続を止めていない）。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        mock.system_prompts().len(),
        1,
        "途中配送失敗後も継続してしまった（§13.1 j: 継続を止めていない）"
    );
    let _ = &buf;
}

// ==================== (#898 §13 #8) reply×N＋本文＋末尾 CONTINUE ====================
//
// DESIGN-TURN-CONTINUATION §13 #8「reply×N＋本文＋最終行 CONTINUE → 配送 reply N＋本文 1・
// 保存 N+1・次 進む・残留なし」。#904 で reply（発話クラス）＋末尾 CONTINUE が次イテレーションへ
// 進むようになったが、併記された**本文（content）**は最終応答と同じ経路で配送・保存される必要が
// ある（本 PR の in-loop 途中発話配送）。現 tip は reply は配送されるが本文（say）が配送も保存も
// されない → 赤。
// 観測境界: extgate の dry-run 配送（kind="reply" / kind="standalone"）・memory_sessions speech・LLM 回数。

const S8_R1: &str = "S8-返信その1";
const S8_R2: &str = "S8-返信その2";
const S8_BODY: &str = "S8-本文。まとめて一言";
const S8_FINAL: &str = "S8-最終本文";

fn two_replies_with_content(r1: &str, r2: &str, content: &str) -> ChatResponse {
    let mut resp = tool_calls_response(vec![
        ("reply", serde_json::json!({"event": "e1", "text": r1})),
        ("reply", serde_json::json!({"event": "e1", "text": r2})),
    ]);
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

struct S8Mock {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for S8Mock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        // 1 回目: reply×2 ＋ 本文 ＋ 末尾 CONTINUE（継続）。2 回目以降: 最終本文で終端。
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(two_replies_with_content(
                S8_R1,
                S8_R2,
                &format!("{S8_BODY}\u{26a1}\nCONTINUE"),
            ))
        } else {
            Ok(text_response(&format!("{S8_FINAL}\u{26a1}")))
        }
    }
}

#[tokio::test]
async fn scenario_s13_8_reply_plus_body_plus_continue_delivers_body_and_continues() {
    let buf = install_capture();
    let mock = Arc::new(S8Mock {
        calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "f8".repeat(32);
    fixture.append_line(&mention_event(
        &ev,
        "S8MARK reply も使いつつ本文も添えて続けて",
    ));

    // (i) reply×2 と本文 say・最終 say がすべて配送される。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            let says = captured(&buf);
            let has = |kind: &str, needle: &str| {
                says.iter()
                    .any(|c| c.kind == kind && c.body.contains(needle))
            };
            has("reply", S8_R1)
                && has("reply", S8_R2)
                && has("standalone", S8_BODY)
                && has("standalone", S8_FINAL)
        })
        .await
    };
    assert!(
        delivered,
        "reply×2＋本文 say＋最終 say のいずれかが配送されない（#8: 本文 say 未配送）: {:?}",
        captured(&buf)
    );

    // (ii) 本文 say（S8_BODY）はちょうど 1 通（1 イテレーション=1 メッセージ）。
    let body_says = captured(&buf)
        .iter()
        .filter(|c| c.kind == "standalone" && c.body.contains(S8_BODY))
        .count();
    assert_eq!(body_says, 1, "本文 say が 1 通でない: {:?}", captured(&buf));

    // (iii) LLM は 2 回（reply＋本文＋CONTINUE で継続 → 2 回目で終端）。
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2, "LLM が 2 回でない");

    // (iv) 残留 CONTINUE なし（配送本文）。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.body.contains("S8-"))
            .all(|c| !c.body.contains("CONTINUE")),
        "配送本文に CONTINUE 残留: {:?}",
        captured(&buf)
    );

    // (v) memory_sessions に本文（S8_BODY）と最終（S8_FINAL）が speech として保存される。
    let speeches: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech")
            .map(|l| l.content)
            .collect()
    };
    assert!(
        speeches.iter().any(|s| s.contains(S8_BODY)),
        "本文（S8_BODY）が memory_sessions に保存されていない: {speeches:?}"
    );
    assert!(
        speeches.iter().any(|s| s.contains(S8_FINAL)),
        "最終本文（S8_FINAL）が memory_sessions に保存されていない: {speeches:?}"
    );
}

// ============ (#899) NO_REPLY のみの応答は speech として保存しない（extgate 境界） ============

/// #899: 配送層（extgate V3）で `NO_REPLY` のみの応答が沈黙になっても、`content='NO_REPLY'`
/// の speech 行が memory_sessions に残り、次ターンの typed 履歴で `assistant: 'NO_REPLY'` として
/// モデルへ渡っていた（`apply_delivery_effect` の NoReply 分岐が `record_agent_no_reply` を
/// 呼んで沈黙マーカーを永続していた）。
///
/// 期待（テンプレ §1・観測境界＝ゲート配送回数/本文・memory_sessions 保存件数/本文・typed 履歴）:
///
/// | シナリオ            | say 配送 | agent speech 保存        | typed の assistant |
/// |---------------------|----------|--------------------------|--------------------|
/// | (a) `NO_REPLY` のみ | 0        | 0（NO_REPLY 行を残さない）| NO_REPLY 無し      |
/// | (b) 本文+`NO_REPLY` | 1（本文）| 1（本文のみ・NO_REPLY 無）| 本文のみ           |
/// | (c) 対照: 通常応答  | 1        | 1                        | 通常応答           |
///
/// (a) が現 tip で赤（`NO_REPLY` 行が保存され typed に現れる）。
#[tokio::test]
async fn scenario_no_reply_only_is_not_persisted_extgate_899() {
    // 一意マーカー（グローバル say バッファの他テスト混線回避）。
    const BODY_B: &str = "NR899B-本文だけ残る";
    const CTRL_C: &str = "NR899C-通常応答";

    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    // FIFO: (a) 単独 NO_REPLY → (b) 本文+NO_REPLY → (d) NO_REPLY+CONTINUE → (c) 対照兼バリア。
    mock.push_text("NO_REPLY");
    mock.push_text(&format!("{BODY_B}\nNO_REPLY"));
    mock.push_text("NO_REPLY\nCONTINUE");
    mock.push_text(CTRL_C);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    // 4 メンションを順に投入（同一 binding＝同一セッション・consumer が FIFO 直列化）。
    // (d) は §13 #13（NO_REPLY+CONTINUE → NO_REPLY 優先で沈黙）。
    fixture.append_line(&mention_event(&"a1".repeat(32), "NR899-mention-a"));
    fixture.append_line(&mention_event(&"b2".repeat(32), "NR899-mention-b"));
    fixture.append_line(&mention_event(&"d4".repeat(32), "NR899-mention-d"));
    fixture.append_line(&mention_event(&"c3".repeat(32), "NR899-mention-c"));

    // バリア: 対照(c)の say が出れば FIFO 上 (a)(b) のターンは決着済み。
    let ok = {
        let buf = buf.clone();
        wait_until(move || captured(&buf).iter().any(|s| s.body.contains(CTRL_C))).await
    };
    assert!(ok, "対照(c)の say が出ない: {:?}", captured(&buf));

    // --- 観測1: ゲート配送（dry-run say） ---
    let says = captured(&buf);
    // (a) 単独 NO_REPLY はどの say にも本文が無い＝配送 0。どの say body にも NO_REPLY が混入しない。
    for s in &says {
        assert!(
            !s.body.contains("NO_REPLY"),
            "say body に NO_REPLY が混入（配送層剥がし漏れ）: {:?}",
            s
        );
    }
    // (b) 本文のみ配送 1・(c) 配送 1。
    assert_eq!(
        says.iter().filter(|s| s.body.contains(BODY_B)).count(),
        1,
        "(b) 本文の say が 1 回配送されていない: {:?}",
        says
    );
    assert_eq!(
        says.iter().filter(|s| s.body.contains(CTRL_C)).count(),
        1,
        "(c) 対照の say が 1 回配送されていない: {:?}",
        says
    );

    // --- 観測2: memory_sessions の agent speech 保存 ---
    let agent_speech: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
            .map(|l| l.content)
            .collect()
    };
    // (a) NO_REPLY のみの生成は speech を残さない。
    assert_eq!(
        agent_speech
            .iter()
            .filter(|c| c.contains("NO_REPLY"))
            .count(),
        0,
        "NO_REPLY を含む agent speech 行が残っている（#899）: {:?}",
        agent_speech
    );
    // (b) 本文だけ 1 行（NO_REPLY 無し）。
    let b_rows: Vec<&String> = agent_speech.iter().filter(|c| c.contains(BODY_B)).collect();
    assert_eq!(
        b_rows.len(),
        1,
        "(b) 本文の保存が 1 行でない: {:?}",
        agent_speech
    );
    assert!(
        !b_rows[0].contains("NO_REPLY"),
        "(b) 保存本文に NO_REPLY が混入: {:?}",
        b_rows[0]
    );
    // (c) 対照 1 行。
    assert_eq!(
        agent_speech.iter().filter(|c| c.contains(CTRL_C)).count(),
        1,
        "(c) 対照の保存が 1 行でない: {:?}",
        agent_speech
    );

    // --- 観測3: 次ターンの typed 履歴に assistant 'NO_REPLY' が無い ---
    let history = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_core::conversation_typed::build_typed_conversation(
            &conn,
            &session_id,
            AGENT_ID,
            200_000,
            100_000,
            false,
            false,
        )
        .unwrap()
        .history
    };
    let assistant_no_reply = history.iter().any(|m| {
        m.role == Role::Assistant
            && m.text_content()
                .map(|t| t.trim() == "NO_REPLY")
                .unwrap_or(false)
    });
    assert!(
        !assistant_no_reply,
        "typed 履歴に assistant 'NO_REPLY' が現れた（#899）: {:?}",
        history
            .iter()
            .map(|m| (
                format!("{:?}", m.role),
                m.text_content().map(|s| s.to_string())
            ))
            .collect::<Vec<_>>()
    );

    // --- 観測4: 各ターンの LLM 呼び出しは 1 回（ターン合計 noreply: LLM==1）---
    // 4 メンション＝4 ターン。各ターンが plain text で 1 生成のみ（沈黙 a/d も含め再生成しない）。
    assert_eq!(
        mock.system_prompts().len(),
        4,
        "各ターンの LLM 呼び出しが 1 回でない（沈黙ターンで余計な再生成が起きている）: {}",
        mock.system_prompts().len()
    );
}
