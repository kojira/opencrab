//! Discord gateway フェーズ1 のオフライン E2E（DESIGN-DISCORD-GATE v17）。
//!
//! トークン・ネットワーク・serenity 接続なしで **実配線** を通す決定的 E2E:
//!   実 `serve_uds`（extgate core）＋ 実 `AppState`（mock LLM）
//!     ⇕ 実 UDS ⇕
//!   実 `discord-gateway::spawn_instance`（fake_events 注入 ＋ dry-run 送信の両有効）
//!
//! 観測channel = dry-run の tracing ログ（target = `opencrab_discordgate::dry_run`）。kind で say /
//! reply / reaction を区別する。単一スレッド（`--test-threads=1`）前提。
//!
//! 検証:
//! - (a) 受信 Discord message → said → turn → say（通常投稿）が dry-run に出る。会話に **e1**（§9A
//!   e番号・core 汎化が discord kind へも採番）が現れる。
//! - (b) reply(e1, 本文) の実 DI 経路: LLM tool_call → core が e1→origin 解決 → invoke → gateway が
//!   REST（dry-run）→ 決着。dry-run に kind="reply" が出る。
//! - (c) reaction(e1, emoji) の実 DI 経路。dry-run に kind="reaction"・emoji が出る。
//!
//! 非 nostr kind の said admission は generic 経路（whitelist + owner/dm）を通る（nostr hooks は張らない）。

use std::path::PathBuf;
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

use opencrab_discord_gateway::config::InstancePlacement;
use opencrab_discord_gateway::harness::HarnessOverrides;
use opencrab_discord_gateway::run::spawn_instance;
use opencrab_extgate::{
    admin_router, resolve_caller_identity_with_owner, serve_uds, ExtgateState, OperatorToken,
};
use opencrab_gate_client::client::InstanceClient;

use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

const TOKEN: &str = "operator-token-discord-qc";
const AGENT_ID: &str = "agent-discord-qc";
const GUILD: &str = "500";
const CHANNEL: &str = "600";
const SELF_BOT: &str = "111"; // bot 自身の user id（自分の投稿除外）。
const AUTHOR: &str = "222"; // owner の Discord user id（generic admission で caller=Owner）。
/// dry-run を拾う tracing target（= `opencrab_discord_gateway::transport::DRY_RUN_LOG_TARGET`）。
const DRY_RUN_TARGET: &str = "opencrab_discordgate::dry_run";

fn address() -> String {
    format!("discord-{AGENT_ID}-{GUILD}-{CHANNEL}")
}

// ==================== 観測: dry-run キャプチャ ====================

#[derive(Clone, Debug, Default)]
struct Captured {
    kind: String,
    body: String,
    emoji: String,
    channel: String,
    message: String,
}

static BUFFER: OnceLock<Arc<Mutex<Vec<Captured>>>> = OnceLock::new();
static INIT: Once = Once::new();

fn install_capture() -> Arc<Mutex<Vec<Captured>>> {
    let buf = BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    INIT.call_once(|| {
        let layer = CaptureLayer { buf: buf.clone() };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    buf
}

struct CaptureLayer {
    buf: Arc<Mutex<Vec<Captured>>>,
}

#[derive(Default)]
struct Visitor {
    kind: Option<String>,
    body: Option<String>,
    emoji: Option<String>,
    channel: Option<String>,
    message: Option<String>,
}

impl Visitor {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "kind" => self.kind = Some(value),
            "body" => self.body = Some(value),
            "emoji" => self.emoji = Some(value),
            "channel" => self.channel = Some(value),
            "message" => self.message = Some(value),
            _ => {}
        }
    }
}

impl tracing::field::Visit for Visitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != DRY_RUN_TARGET {
            return;
        }
        let mut v = Visitor::default();
        event.record(&mut v);
        self.buf.lock().unwrap().push(Captured {
            kind: v.kind.unwrap_or_default(),
            body: v.body.unwrap_or_default(),
            emoji: v.emoji.unwrap_or_default(),
            channel: v.channel.unwrap_or_default(),
            message: v.message.unwrap_or_default(),
        });
    }
}

fn captured(buf: &Arc<Mutex<Vec<Captured>>>) -> Vec<Captured> {
    buf.lock().unwrap().clone()
}

async fn wait_until(pred: impl Fn() -> bool) -> bool {
    for _ in 0..250 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

// ==================== fixture（Discord Message JSONL） ====================

struct Fixture {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();
        Self { path, _dir: dir }
    }

    fn append_message(&self, id: &str, content: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "id": id,
            "channel_id": CHANNEL,
            "guild_id": GUILD,
            "author": {"id": AUTHOR, "bot": false, "username": "owner"},
            "content": content,
        })
        .to_string();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }
}

// ==================== 内容ルーティング mock ====================

struct RoutedMock {
    reqs: Mutex<Vec<String>>,
}

impl RoutedMock {
    fn new() -> Self {
        Self {
            reqs: Mutex::new(Vec::new()),
        }
    }
    fn request_texts(&self) -> Vec<String> {
        self.reqs.lock().unwrap().clone()
    }
}

const M_SAY: &str = "SAYMARK";
const M_REPLY: &str = "REPLYMARK";
const M_REACT: &str = "REACTMARK";
// 他マーカーの部分文字列にならない独立名（"NOREPLYMARK" は "REPLYMARK" を含み誤ルートする）。
const M_NOREPLY: &str = "MUTEMARK";
/// 長文 say を要求するマーカー。`SAYMARK` を部分文字列に含まないので M_SAY と衝突しない。
const M_LONGSAY: &str = "LONGMARK";
/// 長文 say の行数。各行に `LONGSAYLINE{n}` を含め、分割後も全チャンクを識別・再構成できる。
const LONGSAY_LINES: usize = 200;

/// 2000 字を確実に超える複数行本文（1 行ごとに一意トークンを持つ）。
/// mock（送信元）とテスト（期待値）で同じ関数を使い、分割の再構成を厳密照合する。
fn long_say_body() -> String {
    (0..LONGSAY_LINES)
        .map(|i| format!("LONGSAYLINE{i:03}-これは長文分割テストの行です"))
        .collect::<Vec<_>>()
        .join("\n")
}
const B_SAY: &str = "saybody-alpha 通常発言だよ";
const B_REPLY: &str = "replybody-beta 返信本文だよ";
const EMOJI: &str = "👀";
const FILLER: &str = "fillerbody-omega";

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
        self.reqs.lock().unwrap().push(text.clone());
        if !has_tool_role(&request) {
            if text.contains(M_REPLY) {
                return Ok(tool_call_response(
                    "reply",
                    serde_json::json!({"event": "e1", "text": B_REPLY}),
                ));
            }
            if text.contains(M_REACT) {
                return Ok(tool_call_response(
                    "reaction",
                    serde_json::json!({"event": "e1", "emoji": EMOJI}),
                ));
            }
            if text.contains(M_NOREPLY) {
                // 沈黙ターン: say も tool も出さず NO_REPLY だけ返す（core は say 0・ended のみ）。
                return Ok(text_response("NO_REPLY"));
            }
            if text.contains(M_LONGSAY) {
                // 2000 字超の say（turn は plain text で閉じるので resume ループしない）。
                return Ok(text_response(&long_say_body()));
            }
            if text.contains(M_SAY) {
                return Ok(text_response(B_SAY));
            }
        }
        // spawn 後の継続 / 決着後の resume。追加ツールを出さず turn を閉じる filler。
        Ok(text_response(FILLER))
    }
}

// ==================== 共通 helpers（qc_harness_e2e に準拠） ====================

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
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(vec![ToolCall {
            id: format!("tc-{}", uuid::Uuid::new_v4()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }]),
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
            .join("opencrab_discord_qc")
            .to_string_lossy()
            .to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
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
    sock: PathBuf,
    subject_id: i64,
    _dir: tempfile::TempDir,
}

/// 実 serve_uds core + 実 AppState を UDS で立ち上げる。nostr hooks は張らない（discord は generic 経路）。
async fn start_core(provider: Arc<dyn LlmProvider>) -> Core {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    register_mock_pricing(&db);
    let subject_id = upsert_test_agent(&db);
    // discord owner = 発端 author（generic admission で caller=Owner に解決させる）。
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_agent_discord_config(
            &conn,
            &opencrab_db::queries::AgentDiscordConfigRow {
                agent_id: AGENT_ID.into(),
                // legacy 列。V3 gateway は token を env で持つのでここは使わない（placeholder）。
                bot_token: "placeholder-not-used-by-v3".into(),
                owner_discord_id: AUTHOR.into(),
                enabled: true,
            },
        )
        .unwrap();
    }

    let extgate = Arc::new(ExtgateState::new(
        db.clone(),
        OperatorToken::from_bytes(TOKEN),
    ));
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
        sock,
        subject_id,
        _dir: dir,
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
                    "kind_id": "discord",
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

fn discord_config() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "agent_id": AGENT_ID,
        "self_bot_id": SELF_BOT,
        "name": "crab",
        "delivery_mode": "say",
    }))
    .unwrap()
}

/// instance + binding を登録し、discord gateway（fake_events + dry_run）を起動して bind ack を待つ。
async fn wire_instance(core: &Core, fixture: &Fixture) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = address();

    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &addr).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(core.sock.clone(), &place, &config_bytes, None, overrides)
        .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない");
    client
}

fn count_kind(buf: &Arc<Mutex<Vec<Captured>>>, kind: &str) -> usize {
    captured(buf).iter().filter(|c| c.kind == kind).count()
}

// ==================== (a) message → say ＋ §9A e番号 ====================

#[tokio::test]
async fn scenario_a_message_becomes_say_and_conversation_has_e_number() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("700", &format!("{M_SAY} こんにちは、返事して"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(B_SAY))
        })
        .await
    };
    assert!(
        ok,
        "message の say が dry-run に出ない: {:?}",
        captured(&buf)
    );

    // §9A: discord message が会話に e1 として現れる（core 汎化で discord kind に採番）。
    let saw_e1 = {
        let reqs = mock.request_texts();
        reqs.iter().any(|t| t.contains("e1") && t.contains(M_SAY))
    };
    assert!(
        saw_e1,
        "会話に e1（§9A e番号）が現れない（core 汎化が discord に採番していない）: {:?}",
        mock.request_texts()
    );
    // 生 snowflake（channel/message/author id）は会話へ出さない。
    for t in mock.request_texts() {
        assert!(
            !t.contains("discord:message:v1:"),
            "生 origin が会話に露出: {t}"
        );
    }
}

// ==================== (b) reply(e1, 本文) の実 DI 経路 ====================

#[tokio::test]
async fn scenario_b_reply_resolves_e_number_and_settles() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("701", &format!("{M_REPLY} これに返信して"));

    // LLM tool_call reply(e1) → core が e1→origin 解決 → invoke → gateway dry-run reply → 決着。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY))
        })
        .await
    };
    assert!(
        ok,
        "reply の実 DI 経路が決着しない（e1 解決 or invoke or settle 失敗）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        count_kind(&buf, "reply"),
        1,
        "reply が複数回 or 0 回: 自動再送 0"
    );
    // e1 が発端メッセージ（channel=600, message=701）へ正しく解決されている（誤解決検知）。
    let reply = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reply")
        .unwrap();
    assert_eq!(
        reply.channel, CHANNEL,
        "reply 対象 channel が発端と不一致（e1 誤解決）"
    );
    assert_eq!(
        reply.message, "701",
        "reply 対象 message が発端と不一致（e1 誤解決）"
    );
}

// ==================== (c) reaction(e1, emoji) の実 DI 経路 ====================

#[tokio::test]
async fn scenario_c_reaction_resolves_e_number_and_settles() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("702", &format!("{M_REACT} これにリアクションして"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.emoji.contains(EMOJI))
        })
        .await
    };
    assert!(
        ok,
        "reaction の実 DI 経路が決着しない: {:?}",
        captured(&buf)
    );
    assert_eq!(
        count_kind(&buf, "reaction"),
        1,
        "reaction が複数回 or 0 回: 自動再送 0"
    );
    // e1 が発端メッセージ（channel=600, message=702）へ正しく解決されている（誤解決検知）。
    let react = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reaction")
        .unwrap();
    assert_eq!(
        react.channel, CHANNEL,
        "reaction 対象 channel が発端と不一致（e1 誤解決）"
    );
    assert_eq!(
        react.message, "702",
        "reaction 対象 message が発端と不一致（e1 誤解決）"
    );
}

// ==================== (d) system reaction（👀 受理・🏁 完了）の V3 経路 ====================

const SYS_ACCEPTED: &str = "👀";
const SYS_COMPLETED: &str = "🏁";

#[tokio::test]
async fn scenario_d_system_reactions_accepted_and_completed() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // M_SAY: turn は通常発言（say）を返す。agent の reaction DI は使わない
    // （＝ kind="reaction" は 0・system reaction は kind="system_reaction" で分離観測）。
    fixture.append_message("703", &format!("{M_SAY} 受理と完了のサインを見たい"));

    // 👀: LLM がこの発端メッセージをターン文脈に含めた（読んだ）時点＝activity started(origin) で
    // 発端メッセージ（channel=600, message=703）へ付く（R2・受理/推論前では付けない）。
    let saw_accepted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction"
                    && c.emoji.contains(SYS_ACCEPTED)
                    && c.channel == CHANNEL
                    && c.message == "703"
            })
        })
        .await
    };
    assert!(
        saw_accepted,
        "受理 👀（system_reaction）が発端メッセージに付かない: {:?}",
        captured(&buf)
    );

    // 🏁: say を配送し終えた時点で「自分が投稿した say のメッセージ」へ付く（owner 裁定 row 345:
    // 発端ではなく自分の発言に付ける）。say（kind="say"・自分の投稿）の message id と、同じ id に
    // 付いた 🏁（system_reaction）を相関させて検証する。
    let saw_completed_on_own = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            // 自分が投稿した M_SAY ターンの say（本文が B_SAY を含む）の own message id 群。
            let own_say_mids: Vec<String> = caps
                .iter()
                .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(B_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty())
                .collect();
            // その own message id に 🏁 が付いている。
            caps.iter().any(|c| {
                c.kind == "system_reaction"
                    && c.emoji.contains(SYS_COMPLETED)
                    && c.channel == CHANNEL
                    && own_say_mids.contains(&c.message)
            })
        })
        .await
    };
    assert!(
        saw_completed_on_own,
        "完了 🏁（system_reaction）が自分の say メッセージに付かない: {:?}",
        captured(&buf)
    );

    // 付け先誤りの是正: 🏁 は発端メッセージ（703）には**付かない**。
    let completed_on_origin = captured(&buf).iter().any(|c| {
        c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "703"
    });
    assert!(
        !completed_on_origin,
        "🏁 が発端メッセージ 703 に誤って付いている（#869 の付け先取り違え）: {:?}",
        captured(&buf)
    );

    // agent の reaction DI（kind="reaction"）は 703 に対しては起きていない（system reaction を
    // agent reaction と混同していない）。BUFFER は binary 内で共有なので発端 703 に絞って観測する。
    let agent_reactions_on_703 = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reaction" && c.message == "703")
        .count();
    assert_eq!(
        agent_reactions_on_703,
        0,
        "M_SAY ターンで agent reaction が 703 に誤発火: {:?}",
        captured(&buf)
    );

    // 受理 👀 は発端 1 メッセージにつき 1 回（自動再送 0）。
    let accepted_count = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == "703"
        })
        .count();
    assert_eq!(accepted_count, 1, "受理 👀 が複数回: {:?}", captured(&buf));

    // 返信したターン（M_SAY）には 🤐 は付かない（裁定A: core が ended を say の後に出すので
    // 返信ターンで CompletedNoReply が立たない）。
    let noreply_on_703 = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "703")
        .count();
    assert_eq!(
        noreply_on_703,
        0,
        "返信ターンに 🤐 が誤発火（core reorder が効いていない）: {:?}",
        captured(&buf)
    );
}

// ==================== (e) system reaction 🤐（NO_REPLY）の V3 経路 ====================

#[tokio::test]
async fn scenario_e_no_reply_gets_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // M_NOREPLY: turn は NO_REPLY（say 無し）。受理 👀 のあと、沈黙決着で 🤐 が発端へ付く。
    fixture.append_message("704", &format!("{M_NOREPLY} これは黙って"));

    // 🤐: 沈黙ターンの決着（CompletedNoReply・reply_origin=Single）で発端 704 へ付く。
    let saw_muted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction"
                    && c.emoji.contains("🤐")
                    && c.channel == CHANNEL
                    && c.message == "704"
            })
        })
        .await
    };
    assert!(
        saw_muted,
        "NO_REPLY 🤐（system_reaction）が発端メッセージに付かない: {:?}",
        captured(&buf)
    );

    // 沈黙ターンには 🏁（完了）は付かない（返信を配送していない）。
    let completed_on_704 = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "704"
        })
        .count();
    assert_eq!(
        completed_on_704,
        0,
        "NO_REPLY ターンに 🏁 が誤発火: {:?}",
        captured(&buf)
    );
}

// ==================== (f) 2000 字超 say は複数チャンクで逐次配送される ====================

#[tokio::test]
async fn scenario_f_long_say_is_split_into_multiple_chunks() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    let body = long_say_body();
    assert!(
        body.chars().count() > 2000,
        "テスト前提: 本文は 2000 字超（{} 字）",
        body.chars().count()
    );
    let last_token = format!("LONGSAYLINE{:03}", LONGSAY_LINES - 1);

    fixture.append_message("704", &format!("{M_LONGSAY} 長文で返事して"));

    // 逐次送信の完了＝最後の行トークンを含む say チャンクが dry-run に現れる。
    let ok = {
        let buf = buf.clone();
        let last_token = last_token.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(&last_token))
        })
        .await
    };
    assert!(ok, "長文 say の最終チャンクが出ない: {:?}", captured(&buf));

    // このシナリオの say チャンク（LONGSAYLINE を含む）を送信順に収集。
    let chunks: Vec<String> = captured(&buf)
        .into_iter()
        .filter(|c| c.kind == "say" && c.body.contains("LONGSAYLINE"))
        .map(|c| c.body)
        .collect();

    assert!(
        chunks.len() >= 2,
        "2000 字超 say が複数チャンクに分割されていない: {} チャンク",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.chars().count() <= 2000,
            "チャンクが Discord 上限 2000 字を超過: {} 字",
            c.chars().count()
        );
    }
    // 順序保証・欠落なし: チャンクを改行連結すると原文へ戻る（行優先分割）。
    assert_eq!(
        chunks.join("\n"),
        body,
        "分割チャンクの連結が原文と不一致（順序 or 欠落）"
    );
}
