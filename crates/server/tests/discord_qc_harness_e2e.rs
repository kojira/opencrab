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
/// #915: typing 隔離テスト専用チャンネル（他テストは 600 のみ使う）。並列 CI でも typing を分離。
const CHANNEL_TY: &str = "601";
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
    /// #915: reply の own 投稿 id（dry-run が合成・say の message id と同じ形）。reply 以外は空。
    /// `message`（＝返信先 origin id）は従来どおり維持し、🏁 の相関はこの reply_id で行う。
    reply_id: String,
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
    reply_id: Option<String>,
}

impl Visitor {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "kind" => self.kind = Some(value),
            "body" => self.body = Some(value),
            "emoji" => self.emoji = Some(value),
            "channel" => self.channel = Some(value),
            "message" => self.message = Some(value),
            "reply_id" => self.reply_id = Some(value),
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
            reply_id: v.reply_id.unwrap_or_default(),
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
        self.append_message_ch(id, CHANNEL, content);
    }

    /// 指定チャンネルへ発端メッセージを積む（#915: typing 隔離テストが専用チャンネルを使う）。
    fn append_message_ch(&self, id: &str, channel: &str, content: &str) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "id": id,
            "channel_id": channel,
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
// #915: scenario_d 専用の一意 say 本文。BUFFER は binary 内で共有・累積のため、B_SAY（他シナリオ
// でも使う）だと own message id を count で pin できない。独立本文で scenario_d の say を隔離する。
const M_SAY_D: &str = "SAYDMARK";
const B_SAY_D: &str = "saydbody-delta 単発発言だよ（#915 scenario_d 専用）";
const B_REPLY: &str = "replybody-beta 返信本文だよ";
const EMOJI: &str = "👀";
const FILLER: &str = "fillerbody-omega";

// #900 追加マーカー（"REPLYMARK"/"MUTEMARK" 等を部分文字列に含めない独立名）。
const M_REPLY3: &str = "REP3MARK"; // reply×3 in one（§13 #6・reply3-in-one）
const M_REPLY_CONT: &str = "REPCONTMARK"; // reply＋末尾 CONTINUE（§13 #9）
const M_REPLY_NR: &str = "REPSILENTMARK"; // reply＋NO_REPLY（§13 #14）
const B_REPLY3: [&str; 3] = ["rep3-返信1", "rep3-返信2", "rep3-返信3"];
const B_REPLY_CONT: &str = "repcont-返信本文";
const B_REPLY_NR: &str = "repnr-返信本文";

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
        // #900: reply＋末尾 CONTINUE の継続後（tool role あり）は最終 reply で自然終了する。
        // has_tool_role でも先に判定する（継続イテレーションはツール ack を伴う）。
        if text.contains(M_REPLY_CONT) && has_tool_role(&request) {
            return Ok(tool_call_response(
                "reply",
                serde_json::json!({"event": "e1", "text": B_REPLY_CONT}),
            ));
        }
        if !has_tool_role(&request) {
            // §13 #6: reply×3 を 1 生成に並べる（配送 3・LLM 1）。
            if text.contains(M_REPLY3) {
                return Ok(tool_calls_response(
                    B_REPLY3
                        .iter()
                        .map(|b| ("reply", serde_json::json!({"event": "e1", "text": b})))
                        .collect(),
                ));
            }
            // §13 #9: reply＋末尾 CONTINUE（1 生成目・継続する）。
            if text.contains(M_REPLY_CONT) {
                return Ok(reply_with_content_response(B_REPLY_CONT, "CONTINUE"));
            }
            // §13 #14: reply＋NO_REPLY（発話ありの沈黙終端・reply は配送される）。
            if text.contains(M_REPLY_NR) {
                return Ok(reply_with_content_response(B_REPLY_NR, "NO_REPLY"));
            }
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
            if text.contains(M_SAY_D) {
                return Ok(text_response(B_SAY_D));
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

/// 複数 tool_call を 1 生成に並べる（reply×N in one 用）。
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

/// reply tool_call と content を同一生成に載せる（reply＋本文/CONTINUE/NO_REPLY 併記用）。
fn reply_with_content_response(text: &str, content: &str) -> ChatResponse {
    let mut resp = tool_call_response("reply", serde_json::json!({"event": "e1", "text": text}));
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

/// execute_shell tool_call と content（holding 宣言本文）を同一生成に載せる（#916 holding 用）。
/// §13 表 #10「query ツール（execute_shell 等）＋本文（holding）」の 1 生成を作る。
fn shell_with_content_response(content: &str, command: &str, args: &[&str]) -> ChatResponse {
    let mut resp = tool_call_response(
        "execute_shell",
        serde_json::json!({ "command": command, "args": args }),
    );
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
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
    sock: PathBuf,
    subject_id: i64,
    /// #915: execute_shell を実走させる tools_config 等を per-test で触れるよう AppState を保持。
    state: AppState,
    _dir: tempfile::TempDir,
}

/// #915: echo と sleep だけを許可した shell 有効 tools 設定（date 相当＝echo・sleep 相当＝sleep）。
fn shell_enabled_tools_config() -> opencrab_actions::tools::ToolsConfig {
    opencrab_actions::tools::ToolsConfig {
        enabled: true,
        shell: Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["echo".to_string(), "sleep".to_string()],
            ..Default::default()
        }),
    }
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
        state,
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

/// 指定チャンネルで instance+binding を張り gateway を起動する（#915: typing 隔離テスト用）。
/// BUFFER は binary 内で共有・累積かつ CI は並列実行なので、typing（scope key を持たない capture）
/// を他テストと分離するために、他テストが使わない専用チャンネルへ束ねる。
async fn wire_instance_on_channel(
    core: &Core,
    fixture: &Fixture,
    channel: &str,
) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = format!("discord-{AGENT_ID}-{GUILD}-{channel}");

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
    assert!(bound, "binding が ack されない（専用チャンネル {channel}）");
    client
}

fn count_kind(buf: &Arc<Mutex<Vec<Captured>>>, kind: &str) -> usize {
    captured(buf).iter().filter(|c| c.kind == kind).count()
}

/// 指定チャンネルに限定した kind 別キャプチャ数（#915: typing を専用チャンネルで数える）。
fn count_kind_on_channel(buf: &Arc<Mutex<Vec<Captured>>>, kind: &str, channel: &str) -> usize {
    captured(buf)
        .iter()
        .filter(|c| c.kind == kind && c.channel == channel)
        .count()
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

    // §5.4 typing: activity started で gateway が typing を打つ（dry-run kind="typing"）。ターンが
    // 走った（say が出た）＝ started/ended を観測しているので、typing keepalive が最低 1 回打つ。
    let saw_typing = {
        let buf = buf.clone();
        wait_until(move || count_kind(&buf, "typing") >= 1).await
    };
    assert!(
        saw_typing,
        "typing（§5.4）が dry-run に出ない: {:?}",
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
    // 発端 701 に限定して数える（BUFFER は binary 全体で共有・累積するため、他テストの reply
    // 配送を数え込まないよう message で scope する。ハーネス棚卸し・相互汚染の是正）。
    let replies_for_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.message == "701")
        .count();
    assert_eq!(
        replies_for_origin, 1,
        "reply が複数回 or 0 回: 自動再送 0（発端 701 に限定）"
    );
    // e1 が発端メッセージ（channel=600, message=701）へ正しく解決されている（誤解決検知）。
    // BUFFER は共有・累積のため、本テストの reply（body=B_REPLY）に限定して取り出す。
    let reply = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reply" && c.body.contains(B_REPLY))
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
    // 発端 702 に限定して数える（BUFFER は共有・累積のため、他テストの reaction 配送を
    // 数え込まないよう message で scope する。ハーネス棚卸し・相互汚染の是正）。
    let reactions_for_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reaction" && c.message == "702")
        .count();
    assert_eq!(
        reactions_for_origin, 1,
        "reaction が複数回 or 0 回: 自動再送 0（発端 702 に限定）"
    );
    // e1 が発端メッセージ（channel=600, message=702）へ正しく解決されている（誤解決検知）。
    let react = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reaction" && c.message == "702")
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
    fixture.append_message("703", &format!("{M_SAY_D} 受理と完了のサインを見たい"));

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

    // 🏁: DESIGN-TURN-CONTINUATION §13.2「activity ended を受けた時点で、そのターンで最後に
    // 成功した自分の say メッセージに 1 件だけ」。単発 say ターンなので最後の投稿 ＝ この 1 件の say。
    // §13.2 表 row 1（ターンが投稿で終わる → 最後の投稿に 1）。own message id で相関し、`any` では
    // なく**総数 == 1**で pin する（say ごとに 🏁 を付ける実装なら総数が増える → 検知）。
    let own_say_mids: Vec<String> = {
        let wbuf = buf.clone();
        // 最後の say（＝この単発 say）の own message id に 🏁 が付くまで待つ。
        wait_until(move || {
            let caps = captured(&wbuf);
            let mids: Vec<String> = caps
                .iter()
                .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(B_SAY_D))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty())
                .collect();
            !mids.is_empty()
                && caps.iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.channel == CHANNEL
                        && mids.contains(&c.message)
                })
        })
        .await;
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(B_SAY_D))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
            .collect()
    };
    assert_eq!(
        own_say_mids.len(),
        1,
        "単発 say の own message id が 1 件でない: {:?}",
        captured(&buf)
    );
    let completed_on_own = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction"
                && c.emoji.contains(SYS_COMPLETED)
                && c.channel == CHANNEL
                && own_say_mids.contains(&c.message)
        })
        .count();
    assert_eq!(
        completed_on_own,
        1,
        "完了 🏁 が自分の最後の say に 1 件で付かない（§13.2・ターン終了時のみ）: {:?}",
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

    // §13.2 表 row 11/13（NO_REPLY のみ → 🏁 0・沈黙終了は 🤐）。沈黙ターンは自分の投稿が無い
    // ので 🏁 は付かない。発端 704 への誤付与を総数 0 で pin する（この turn の自投稿 id は無い）。
    let completed_on_704 = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "704"
        })
        .count();
    assert_eq!(
        completed_on_704,
        0,
        "NO_REPLY ターンに 🏁 が誤発火（§13.2 row 11/13）: {:?}",
        captured(&buf)
    );

    // #899 / §12.6: 沈黙決着で 🤐 は付くが、speech='NO_REPLY' の監査行は残さない。
    // （裸 NO_REPLY を永続すると conversation_typed が assistant 'NO_REPLY' として再注入する。）
    let no_reply_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content='NO_REPLY'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        no_reply_rows, 0,
        "NO_REPLY のみのターンで speech='NO_REPLY' が保存された（#899・§12.6）: {no_reply_rows}"
    );
}

// ==================== (g) reply×N＋NO_REPLY（§13 #14）: reply 保存・NO_REPLY 行なし・🤐なし ====================

/// §13 #14: 発話 op（reply）を出したターンの末尾が NO_REPLY でも、reply は配送/保存され、
/// 末尾 NO_REPLY は speech 行を足さない（#899）。発話があるので 🤐 は付かない。
///
/// | 観測点 | 期待 |
/// |---|---|
/// | reply 配送（dry-run kind=reply） | 1（本文 B_REPLY_NR） |
/// | speech='NO_REPLY' 保存 | 0 |
/// | 🤐（system_reaction）on 発端 | 0（発話ありターン） |
///
/// 現 tip で赤: 末尾 NO_REPLY が record_agent_no_reply で `content='NO_REPLY'` を保存する。
#[tokio::test]
async fn scenario_g_reply_then_no_reply_saves_reply_not_no_reply() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // 705: 同一生成で reply op ＋ content=NO_REPLY（#904 の M_REPLY_NR 契約）。
    fixture.append_message("705", &format!("{M_REPLY_NR} 返信してから黙って"));

    // reply（一意本文）が配送されるまで待つ。on_tool_call の content 保存は reply 実行の
    // **前**に走るので、この時点で（現 tip なら）NO_REPLY 行は既に書かれている。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR))
        })
        .await
    };
    assert!(ok, "reply(705) が配送されない: {:?}", captured(&buf));

    // reply は 1 回だけ（一意本文なので他テスト混線なし）。
    let reply_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR))
        .count();
    assert_eq!(
        reply_count,
        1,
        "reply が 1 回配送されていない（§13 #14）: {:?}",
        captured(&buf)
    );

    // #899: reply があっても末尾 / 同一生成の NO_REPLY は speech='NO_REPLY' 行を足さない
    //（on_tool_call の content 保存経路・配送層 NoReply の両方）。
    let no_reply_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content='NO_REPLY'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        no_reply_rows, 0,
        "reply×N＋NO_REPLY のターンで speech='NO_REPLY' が保存された（#899・§13 #14）: {no_reply_rows}"
    );
    // 注: 「発話ありターンに 🤐 を付けない」は既存 scenario_d が担保（本テストは #899 の保存側に集中）。
}

// ==================== (e2) reply（発話 invoke）ターンには 🤐 が付かない（#900） ====================
//
// #900: reply/reaction は say ではなく invoke で配送される。gate は「発話（say/reply/reaction）が
// 1 つでもあれば沈黙ではない」と判定すべきで、reply しかしていないターンを最終本文空＝沈黙と解釈して
// 🤐 を付けてはならない。reply 配送後、ターン決着（activity ended）で 🤐 が付くならその時点で出るので、
// 🤐 の出現を bounded poll で待って「出ない」ことを確定する（バグ時は即座に 🤐 が出て RED）。
#[tokio::test]
async fn scenario_e2_reply_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // 701: reply ターン（発話は invoke 経路・say 無し）。
    fixture.append_message("701", &format!("{M_REPLY} これに返信して"));
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY) && c.message == "701")
        })
        .await
    };
    assert!(replied, "reply が 701 へ配送されない: {:?}", captured(&buf));

    // 701（reply ターン）に 🤐 が付かない。バグ時は決着で 🤐 が即座に出るので wait_until が true に
    // なって RED。修正後は発話ありと判定されて 🤐 が出ず、poll は timeout して false（GREEN）。
    let muted_on_701 = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "701"
            })
        })
        .await
    };
    assert!(
        !muted_on_701,
        "reply ターン(701)に 🤐 が誤発火（発話を沈黙扱いした）: {:?}",
        captured(&buf)
    );
}

/// 発端 origin に 🤐（system_reaction）が付いていないことを bounded poll で確定する共通ヘルパー。
/// バグ時は決着で 🤐 が即座に出て poll が true→assert 失敗（RED）。修正後は 🤐 が出ず false（GREEN）。
async fn assert_no_muted_on(buf: &Arc<Mutex<Vec<Captured>>>, message: &str) {
    let b = buf.clone();
    let m = message.to_string();
    let muted = wait_until(move || {
        captured(&b)
            .iter()
            .any(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == m)
    })
    .await;
    assert!(
        !muted,
        "発話ありターン({message})に 🤐 が誤発火（発話を沈黙扱いした）: {:?}",
        captured(buf)
    );
}

// ==================== (e3) §13 #6: reply×3 in one ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e3_reply3_in_one_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("706", &format!("{M_REPLY3} 3回に分けて返信して"));
    let all_replied = {
        let buf = buf.clone();
        wait_until(move || {
            B_REPLY3.iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && c.message == "706")
            })
        })
        .await
    };
    assert!(
        all_replied,
        "reply×3 in one が全配送されない: {:?}",
        captured(&buf)
    );
    // 発話（reply）が 3 つあったので沈黙ではない → 🤐 は付かない（§13 #6）。
    assert_no_muted_on(&buf, "706").await;
}

// ==================== (e4) §13 #9: reply＋末尾 CONTINUE ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e4_reply_then_continue_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("707", &format!("{M_REPLY_CONT} 返信して続けて"));
    // 継続後の最終 reply も同じ 707 へ配送される（継続が起きた証拠）。少なくとも reply が届く。
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_CONT) && c.message == "707")
        })
        .await
    };
    assert!(
        replied,
        "reply＋CONTINUE ターンの reply が配送されない: {:?}",
        captured(&buf)
    );
    // 発話（reply）があったので 🤐 は付かない（§13 #9）。
    assert_no_muted_on(&buf, "707").await;
}

// ==================== (e5) §13 #14: reply＋NO_REPLY ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e5_reply_then_no_reply_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("708", &format!("{M_REPLY_NR} 返信して黙って"));
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR) && c.message == "708")
        })
        .await
    };
    assert!(
        replied,
        "reply＋NO_REPLY ターンの reply が配送されない: {:?}",
        captured(&buf)
    );
    // 最終本文は NO_REPLY だが、そのターンに reply（発話）があるので 🤐 は付かない（§13 #14）。
    assert_no_muted_on(&buf, "708").await;
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

// =====================================================================================
// 監査ピン #900(a/c)【§13 #6（reply×N 本文なし→reply N 配送/保存 N/🤐 付けない）／ターン合計 reply3-in-one・
// §13.1 g（reaction/repost のみも #6 と同じ）の reply 版】: reply×3-in-one（発話クラスのみのターン）→ 配送 3・LLM 1・🤐 なし。
//
// 現 tip: reply×3 は撃ちっぱなしで配送されるが、最終本文が空（say 0）のためゲートが沈黙と解釈し
// CompletedNoReply → 🤐 を発端へ付ける（#883 発話クラス化の契約列挙漏れ）。→ 🤐 の pin で赤。
// 期待: 発話（say/reply/reaction）が 1 つでもあったターンには 🤐 を付けない。
//
// 既存 scenario_a3_three_replies_...（qc_harness_e2e）は配送 3・LLM 1 を pin 済みだが 🤐 反応は
// 観測していない。ここは discord ゲートの system reaction を観測できる唯一のハーネスなので
// 「足りない観測点＝🤐 なし」だけを追加する。
// =====================================================================================
const M_AUDIT_REPLY3: &str = "AUDITREPLY3MARK";
const B_R3_1: &str = "r3body-one 一通目だよ";
const B_R3_2: &str = "r3body-two 二通目だよ";
const B_R3_3: &str = "r3body-three 三通目だよ";

struct ThreeReplyMock {
    calls: std::sync::atomic::AtomicUsize,
}

fn three_reply_response() -> ChatResponse {
    let tc = |text: &str| ToolCall {
        id: format!("tc-{}", uuid::Uuid::new_v4()),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "reply".to_string(),
            arguments: serde_json::json!({"event": "e1", "text": text}).to_string(),
        },
    };
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(vec![tc(B_R3_1), tc(B_R3_2), tc(B_R3_3)]),
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

#[async_trait::async_trait]
impl LlmProvider for ThreeReplyMock {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let text = request_text(&request);
        if !has_tool_role(&request) && text.contains(M_AUDIT_REPLY3) {
            return Ok(three_reply_response());
        }
        // 撃ちっぱなしなので追加ターンは来ないはずだが、保険で沈黙終端。
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn audit_900c_utterance_only_reply_turn_gets_no_muted_reaction() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ThreeReplyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("900", &format!("{M_AUDIT_REPLY3} 3回に分けて返信して"));

    // reply×3 がすべて配送されるまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            [B_R3_1, B_R3_2, B_R3_3].iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b))
            })
        })
        .await
    };
    assert!(delivered, "reply×3 が配送されない: {:?}", captured(&buf));

    // 🤐 判定は決着時に立つので、決着の猶予を置く。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 配送 3: reply が 3 通。
    for b in [B_R3_1, B_R3_2, B_R3_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(b))
            .count();
        assert_eq!(
            n,
            1,
            "reply {b} の配送回数が 1 でない: {:?}",
            captured(&buf)
        );
    }

    // LLM 1 回（reply×3 は 1 生成に並ぶ・ack ごとの再呼び出しなし）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        1,
        "reply×3 が 1 生成で完了していない（LLM 呼び出しが 1 でない）"
    );

    // 🤐 なし: 発話があったターンなので発端 900 に 🤐 は付かない（現 tip は付く → 赤）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "900")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話（reply×3）があったターンに 🤐 が誤発火（#900: 発話クラスの契約列挙漏れ）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// §13.1 c【Discord で 1 イテレーション = 1 メッセージ（結合/編集しない）】: 本文＋CONTINUE で
// 3 分割 → Discord に 3 メッセージが別々に出る（#898 の discord レーン版）。
// 現 tip: 配送層が最終応答（er.response）だけを say するので最後の 1 メッセージだけ → 赤。
// §13 #2 を 3 連鎖／ターン合計 plain3 の Discord レーン。
// ---------------------------------------------------------------------------
const CC_1: &str = "CCsplit-one 一通目の本文";
const CC_2: &str = "CCsplit-two 二通目の本文";
const CC_3: &str = "CCsplit-three 三通目の本文";

struct ContinueSplitMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ContinueSplitMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            0 => text_response(&format!("{CC_1}\nCONTINUE")),
            1 => text_response(&format!("{CC_2}\nCONTINUE")),
            _ => text_response(CC_3),
        })
    }
}

#[tokio::test]
async fn audit_s13_1c_continue_split_is_separate_discord_messages() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ContinueSplitMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("1301", "CCMARK 3回に分けて投稿して reply使わずに");

    // 最終メッセージ（CC_3）が出るまで待つ（= 3 イテレーションに到達）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(CC_3))
        })
        .await
    };
    assert!(done, "3 通目（最終）が出ない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 各イテレーションが別々の 1 メッセージとして出る（現 tip は CC_3 のみ → 赤）。
    for m in [CC_1, CC_2, CC_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.body.contains(m))
            .count();
        assert_eq!(
            n,
            1,
            "分割 {m} の Discord メッセージが 1 通でない（#898 discord: 途中発話未配送）: {:?}",
            captured(&buf)
        );
    }
    // 結合していない: どの 1 メッセージも 2 マーカーを同時に含まない。
    assert!(
        captured(&buf).iter().filter(|c| c.kind == "say").all(|c| {
            [CC_1, CC_2, CC_3]
                .iter()
                .filter(|m| c.body.contains(*m))
                .count()
                <= 1
        }),
        "分割メッセージが 1 通に結合された（1 イテレーション=1 メッセージに反する）: {:?}",
        captured(&buf)
    );
    // LLM 3・残留 CONTINUE なし。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        3,
        "CONTINUE 3 分割の LLM 呼び出しが 3 でない"
    );
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && [CC_1, CC_2, CC_3].iter().any(|m| c.body.contains(m)))
            .all(|c| !c.body.contains("CONTINUE")),
        "say に CONTINUE が残留: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915【🏁 はターン終了時のみ・途中投稿には付けない】: 純 say の末尾 CONTINUE で 3 分割した
// ターン（本文＋CONTINUE ×2 → 本文）で、🏁（完了サイン）は**最後の say メッセージ 1 件だけ**に
// 付き、途中の 2 件には付かない。オーナー裁定（逐語）:「🏁を付けるのは次のターンがない時だけ
// です」「続きがないことを知らせるものですよ」。
//
// 観測境界（dry-run capture）: kind="say" の各分割メッセージの own message id と、kind=
// "system_reaction"・emoji=🏁 の付け先 message を相関する。現 tip は say 配送ごとに 🏁 を付ける
// ため途中 2 件にも 🏁 が付く → 赤。修正後は activity ended で最後の say だけに付く → 緑。
// LLM 3・say 配送 3・🏁 は最終 1 件のみ・途中 0 件・🤐 0（発話ありターン）。
// ---------------------------------------------------------------------------
const FC_1: &str = "flagcont-one 一通目";
const FC_2: &str = "flagcont-two 二通目";
const FC_3: &str = "flagcont-three 三通目";

struct FlagContinueSplitMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for FlagContinueSplitMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            0 => text_response(&format!("{FC_1}\nCONTINUE")),
            1 => text_response(&format!("{FC_2}\nCONTINUE")),
            _ => text_response(FC_3),
        })
    }
}

#[tokio::test]
async fn scenario_915_completed_flag_only_on_last_say_of_continue_split() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(FlagContinueSplitMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("915", "FCMARK 3回に分けて返信して reply使わずに");

    // 最終メッセージ（FC_3）が say として出るまで待つ（= 3 イテレーションに到達・say 配送 3）。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FC_3))
        })
        .await
    };
    assert!(done, "3 通目（最終 say）が出ない: {:?}", captured(&buf));

    // 最終 say（FC_3）の own message id に 🏁 が付くまで待つ（決着＝activity ended で付与）。
    // ヘルパ: 本文で分割 say を特定し own message id を返す（BUFFER は共有なので本文で scope）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let saw_last_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let last_mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FC_3))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match last_mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        saw_last_completed,
        "🏁 が最終 say（FC_3）に付かない: {:?}",
        captured(&buf)
    );
    // 決着後、途中投稿の誤付与が無いことを確定するための猶予（バグ時は配送ごとに即付くので既に出ている）。
    tokio::time::sleep(Duration::from_millis(400)).await;

    let first_mid = mid_of(FC_1).expect("FC_1 say の message id");
    let second_mid = mid_of(FC_2).expect("FC_2 say の message id");
    let third_mid = mid_of(FC_3).expect("FC_3 say の message id");

    let completed_on = |mid: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == mid
            })
            .count()
    };

    // 🏁 は最終 say（FC_3）1 件のみ。途中の 2 件（FC_1/FC_2）には付かない（現 tip は付く → 赤）。
    assert_eq!(
        completed_on(&first_mid),
        0,
        "🏁 が途中 say FC_1 に誤って付いている（ターン終了時のみの裁定違反）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&second_mid),
        0,
        "🏁 が途中 say FC_2 に誤って付いている（ターン終了時のみの裁定違反）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&third_mid),
        1,
        "🏁 が最終 say FC_3 に 1 件付かない: {:?}",
        captured(&buf)
    );

    // ターン全体での 🏁 総数は 1（分割 3 メッセージ合計）。
    let total_completed: usize = [&first_mid, &second_mid, &third_mid]
        .iter()
        .map(|m| completed_on(m))
        .sum();
    assert_eq!(
        total_completed,
        1,
        "分割ターンの 🏁 総数が 1 でない（途中付与のバグ）: {:?}",
        captured(&buf)
    );

    // 発話ありターンなので発端 915 に 🤐 は付かない（回帰）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "915")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話ありターンに 🤐 が誤発火: {:?}",
        captured(&buf)
    );

    // LLM 3・say 配送 3・CONTINUE 残留なし（回帰）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        3,
        "CONTINUE 3 分割の LLM 呼び出しが 3 でない"
    );
    for m in [FC_1, FC_2, FC_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(m))
            .count();
        assert_eq!(n, 1, "分割 {m} の say が 1 通でない: {:?}", captured(&buf));
    }
}

// ---------------------------------------------------------------------------
// #915 / §13.2 表 row 8【reply → CONTINUE → say】: reply を配送してから CONTINUE で継続し、
// 最終イテレーションで say を投稿するターン。🏁 はターン終了時（activity ended）の最後の投稿
// ＝最終 say に **1 件だけ**。途中の reply には付けない（own say id で相関・count で pin）。
// ---------------------------------------------------------------------------
const RC_REPLY: &str = "rcreply-途中の返信本文";
const RC_SAY: &str = "rcsay-最終の通常発言";

struct ReplyThenContinueThenSayMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyThenContinueThenSayMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            // 1 生成目: reply（本文 RC_REPLY）＋末尾 CONTINUE（本文は空＝継続のみ）→ 進む。
            0 => reply_with_content_response(RC_REPLY, "CONTINUE"),
            // 2 生成目: 純 say（最終・CONTINUE なし）→ ターン終了。
            _ => text_response(RC_SAY),
        })
    }
}

#[tokio::test]
async fn scenario_915_reply_then_continue_then_say_flag_only_on_last_say() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReplyThenContinueThenSayMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9153", &format!("{M_REPLY} からの CONTINUE で最後は say"));

    // 最終 say（RC_SAY）の own message id に 🏁 が付くまで待つ（決着＝activity ended で付与）。
    let saw_last_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let last_mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(RC_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match last_mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        saw_last_completed,
        "🏁 が最終 say（RC_SAY）に付かない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(400)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(RC_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("RC_SAY say の message id");

    // 🏁 は最終 say に 1 件のみ（§13.2 row 8）。
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        1,
        "🏁 が最終 say に 1 件で付かない（§13.2 row 8）: {:?}",
        captured(&buf)
    );

    // 途中の reply は配送された（kind="reply"・own reply_id あり）。
    let reply_id = captured(&buf)
        .iter()
        .find(|c| c.kind == "reply" && c.body.contains(RC_REPLY))
        .map(|c| c.reply_id.clone())
        .filter(|m| !m.is_empty())
        .expect("途中 reply の reply_id");
    // §13.3.6 row 9（非ブロック指摘・DIRECTION-LOG 追加）: 途中 reply の reply_id には 🏁 0。
    let completed_on_reply = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == reply_id
        })
        .count();
    assert_eq!(
        completed_on_reply,
        0,
        "🏁 が途中 reply（reply_id）に誤付与（最終イテレーションの投稿のみ・§13.3.6 row 9）: {:?}",
        captured(&buf)
    );
    // 発端 9153（reply 先）にも付けない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9153"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が発端 9153（reply 先）に誤って付いている（§13.3.6）: {:?}",
        captured(&buf)
    );
    // ターン全体で 🏁 は 1（最終 say のみ・途中 reply/発端は 0）。
    let total = completed_on_say + completed_on_reply + completed_on_origin;
    assert_eq!(
        total,
        1,
        "reply→CONTINUE→say の 🏁 総数が 1 でない（§13.3.6 row 9）: {:?}",
        captured(&buf)
    );

    // LLM 2 回（reply+CONTINUE → 最終 say）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "reply→CONTINUE→say の LLM 呼び出しが 2 でない"
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.2・DIRECTION-LOG 446【say→CONTINUE→NO_REPLY で終わるターン】: 最終生成が NO_REPLY
// （投稿なし）なので 🏁 は 0。途中の say（CONTINUE で進んだ投稿）にも付けない。発話があった
// ターンなので 🤐 も 0。ルール: 🏁 は「ツール呼び出しも CONTINUE も含まない最終生成の自分の
// 投稿」にだけ付く。最終生成に投稿が無ければ付けない。
// ---------------------------------------------------------------------------
// 本文中に "NO_REPLY"/"CONTINUE" の部分文字列を含めない（含めるとサニタイザに途中で切られ、
// 継続ではなく本文＋末尾 NO_REPLY（row 12）扱いになる）。
const SCN_SAY: &str = "scncont-途中で続ける本文だよ";

struct SayContinueThenNoReplyMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SayContinueThenNoReplyMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            // 1 生成目: 本文＋末尾 CONTINUE（途中の投稿・継続）。
            0 => text_response(&format!("{SCN_SAY}\nCONTINUE")),
            // 2 生成目: NO_REPLY（投稿なしで終端）。
            _ => text_response("NO_REPLY"),
        })
    }
}

#[tokio::test]
async fn scenario_915_say_continue_then_no_reply_gets_no_flag() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(SayContinueThenNoReplyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9160", "SCNMARK 続けてから最後は黙る");

    // 途中 say（SCN_SAY）が配送されるまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SCN_SAY))
        })
        .await
    };
    assert!(
        delivered,
        "途中 say（SCN_SAY）が配送されない: {:?}",
        captured(&buf)
    );
    // 最終 NO_REPLY 決着まで猶予（🏁/🤐 の付与はターン終了時）。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SCN_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("SCN_SAY say の message id");

    // 🏁 は途中 say に付かない（最終生成は NO_REPLY＝投稿なし → 🏁 0）。現 tip は say 配送ごとに
    // 付けるため途中 say に 🏁 が付く → 赤。
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        0,
        "🏁 が途中 say に誤付与（最終 NO_REPLY のターンは 🏁 0・DIRECTION-LOG 446）: {:?}",
        captured(&buf)
    );
    // 発端 9160 にも 🏁 は付かない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9160"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が発端に誤付与: {:?}",
        captured(&buf)
    );

    // 発話（途中 say）があったターンなので 🤐 も 0（沈黙終了ではない）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9160")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "🏁 発話ありターンに 🤐 が誤付与（say→CONTINUE→NO_REPLY は 🤐 0）: {:?}",
        captured(&buf)
    );

    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "say→CONTINUE→NO_REPLY の LLM 呼び出しが 2 でない"
    );
}

// ---------------------------------------------------------------------------
// #915 / DIRECTION-LOG 446【reply→CONTINUE→reaction のみで終わるターン】: 最終生成が reaction のみ
// （reaction は「投稿」ではない）なので 🏁 は 0。途中の reply（CONTINUE で進んだ投稿）にも付けない。
// reply/reaction は invoke 経路で say consumer を通らないため現 tip でも 0（回帰ガード）。
// ---------------------------------------------------------------------------
const RR_REPLY: &str = "rrreply-途中の返信（最終は reaction のみ）";
const RR_EMOJI: &str = "✅";

struct ReplyContinueThenReactionMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyContinueThenReactionMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match n {
            // 1 生成目: reply（本文 RR_REPLY）＋末尾 CONTINUE（本文は空＝継続）→ 進む。
            0 => reply_with_content_response(RR_REPLY, "CONTINUE"),
            // 2 生成目: reaction のみ（投稿なしで終端）。
            _ => tool_call_response(
                "reaction",
                serde_json::json!({"event": "e1", "emoji": RR_EMOJI}),
            ),
        })
    }
}

#[tokio::test]
async fn scenario_915_reply_continue_then_reaction_only_gets_no_flag() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReplyContinueThenReactionMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9161", "RRMARK 返信してから最後はリアクションだけ");

    // 最終 reaction（RR_EMOJI・発端 9161）が配送されるまで待つ。
    let reacted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.emoji.contains(RR_EMOJI) && c.message == "9161")
        })
        .await
    };
    assert!(
        reacted,
        "最終 reaction が配送されない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 途中 reply は配送された。
    let reply_delivered = captured(&buf)
        .iter()
        .any(|c| c.kind == "reply" && c.body.contains(RR_REPLY));
    assert!(
        reply_delivered,
        "途中 reply が配送されない: {:?}",
        captured(&buf)
    );

    // 🏁 は 0（最終生成は reaction のみ＝投稿なし）。発端 9161 にも付かない。
    let completed_on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9161"
        })
        .count();
    assert_eq!(
        completed_on_origin,
        0,
        "🏁 が reaction 終端ターンに誤付与（最終生成 reaction のみ → 🏁 0）: {:?}",
        captured(&buf)
    );
    // 発話ありターンなので 🤐 も 0。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9161")
        .count();
    assert_eq!(muted_on_origin, 0, "🤐 が誤付与: {:?}", captured(&buf));

    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        2,
        "reply→CONTINUE→reaction の LLM 呼び出しが 2 でない"
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 6【reply×N（本文なし・単一生成）→ 最後の reply に 1】: 1 生成で reply を 3 本
// 出すターン。🏁 は最後の reply の own 投稿 id（reply_id）に 1・他 0・総数 1。reply は invoke 経路
// なので現 tip は 🏁 0 → **赤**。相関は capture の `reply_id`（dry-run が合成した own 投稿 id）で行う
// （`message`＝返信先 origin とは分離）。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn scenario_915_reply3_flag_on_last_reply() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9170", &format!("{M_REPLY3} 3 本まとめて返信して"));

    // reply 3 本が配送されるまで待つ（各 reply は distinct な reply_id を持つ）。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            B_REPLY3.iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && !c.reply_id.is_empty())
            })
        })
        .await
    };
    assert!(delivered, "reply×3 が配送されない: {:?}", captured(&buf));
    // 決着（ended）まで猶予。🏁 はターン終了時付与。
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 各 reply の own 投稿 id（reply_id）を本文で引く。
    let reply_id_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "reply" && c.body.contains(body))
            .map(|c| c.reply_id.clone())
            .filter(|m| !m.is_empty())
    };
    let last_reply_id = reply_id_of(B_REPLY3[2]).expect("最後の reply の reply_id");
    let first_reply_id = reply_id_of(B_REPLY3[0]).expect("1 本目 reply の reply_id");
    let second_reply_id = reply_id_of(B_REPLY3[1]).expect("2 本目 reply の reply_id");

    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 🏁 は最後の reply に 1・途中 2 本には 0（§13.3.6 row 6）。現 tip は reply に 🏁 0 → 赤。
    assert_eq!(
        completed_on(&last_reply_id),
        1,
        "🏁 が最後の reply に 1 件で付かない（§13.3.6 row 6・現 tip は reply に 🏁 0）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&first_reply_id),
        0,
        "🏁 が 1 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&second_reply_id),
        0,
        "🏁 が 2 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    let total: usize = [&first_reply_id, &second_reply_id, &last_reply_id]
        .iter()
        .map(|id| completed_on(id))
        .sum();
    assert_eq!(
        total,
        1,
        "reply×3 ターンの 🏁 総数が 1 でない: {:?}",
        captured(&buf)
    );

    // 発端 9170 にも 🏁 は付かない（🏁 は自分の投稿へ）。
    let on_origin = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "9170"
        })
        .count();
    assert_eq!(on_origin, 0, "🏁 が発端に誤付与: {:?}", captured(&buf));
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 14【reply×N + NO_REPLY（同一生成）→ 最後の reply に 1】: 1 生成で reply を 2 本
// ＋本文 NO_REPLY。reply は配送され、最終応答は NO_REPLY（say なし）。🏁 は最後の reply の reply_id
// に 1・他 0・総数 1。発話ありなので 🤐 0。reply は invoke 経路で現 tip は 🏁 0 → **赤**。
// ---------------------------------------------------------------------------
const RNR_1: &str = "rnr-返信1";
const RNR_2: &str = "rnr-返信2（最後）";

struct Reply2ThenNoReplyMock;

#[async_trait::async_trait]
impl LlmProvider for Reply2ThenNoReplyMock {
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
        let mut resp = tool_calls_response(vec![
            ("reply", serde_json::json!({"event": "e1", "text": RNR_1})),
            ("reply", serde_json::json!({"event": "e1", "text": RNR_2})),
        ]);
        resp.choices[0].message.content = Some(MessageContent::Text("NO_REPLY".to_string()));
        Ok(resp)
    }
}

#[tokio::test]
async fn scenario_915_reply2_then_no_reply_flag_on_last_reply() {
    let buf = install_capture();
    let mock = Arc::new(Reply2ThenNoReplyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9180", "RNRMARK 2 本返信して最後は黙る");

    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            [RNR_1, RNR_2].iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && !c.reply_id.is_empty())
            })
        })
        .await
    };
    assert!(delivered, "reply×2 が配送されない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(600)).await;

    let reply_id_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "reply" && c.body.contains(body))
            .map(|c| c.reply_id.clone())
            .filter(|m| !m.is_empty())
    };
    let last_id = reply_id_of(RNR_2).expect("最後の reply の reply_id");
    let first_id = reply_id_of(RNR_1).expect("1 本目 reply の reply_id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&last_id),
        1,
        "🏁 が最後の reply に 1 件で付かない（§13.3.6 row 14・現 tip は reply に 🏁 0）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&first_id),
        0,
        "🏁 が 1 本目の reply に誤付与: {:?}",
        captured(&buf)
    );
    // 発話（reply）ありなので発端 9180 に 🤐 は付かない。
    let muted = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9180")
        .count();
    assert_eq!(
        muted,
        0,
        "reply ありターンに 🤐 が誤付与: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 12【本文 + 末尾 NO_REPLY → 最終 say に 1】: 本文は配送され（NO_REPLY 以降は破棄）、
// 最終生成の投稿＝その say。🏁 はその say に 1。現 tip も say に付く（回帰ガード）。
// ---------------------------------------------------------------------------
// 本文中に "NO_REPLY"/"CONTINUE" の部分文字列を含めない（サニタイザに途中で切られないため）。
const BNR_SAY: &str = "bnrsay-末尾マーカーで黙る本文だよ";

struct BodyThenTrailingNoReplyMock;

#[async_trait::async_trait]
impl LlmProvider for BodyThenTrailingNoReplyMock {
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
        Ok(text_response(&format!("{BNR_SAY}\nNO_REPLY")))
    }
}

#[tokio::test]
async fn scenario_915_body_then_trailing_no_reply_flag_on_say() {
    let buf = install_capture();
    let mock = Arc::new(BodyThenTrailingNoReplyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9181", "BNRMARK 本文を出して末尾で黙る");

    let say_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(BNR_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        say_completed,
        "本文 say＋🏁 が観測できない（本文が配送されないか 🏁 が付かない）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(BNR_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("BNR_SAY say の message id");
    let completed = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed,
        1,
        "🏁 が本文 say に 1 件で付かない（§13.3.6 row 12）: {:?}",
        captured(&buf)
    );
    // 本文に NO_REPLY マーカーが残らない（回帰）。
    assert!(
        !captured(&buf)
            .iter()
            .any(|c| c.kind == "say" && c.body.contains("NO_REPLY")),
        "say 本文に NO_REPLY が残留: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 7【reply×N + 本文（同一生成）→ 到着順で最後の投稿に 1】: 1 生成で reply×2＋本文。
// reply は invoke で先に配送、本文 say は最終応答として後に配送＝到着順で最後は say。🏁 はその say に
// 1・reply には 0。現 tip も say に付く（回帰ガード）。
// ---------------------------------------------------------------------------
const R7_1: &str = "r7-返信1";
const R7_2: &str = "r7-返信2";
const R7_SAY: &str = "r7say-本文（到着順で最後）";

struct Reply2PlusBodyMock;

#[async_trait::async_trait]
impl LlmProvider for Reply2PlusBodyMock {
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
        let mut resp = tool_calls_response(vec![
            ("reply", serde_json::json!({"event": "e1", "text": R7_1})),
            ("reply", serde_json::json!({"event": "e1", "text": R7_2})),
        ]);
        resp.choices[0].message.content = Some(MessageContent::Text(R7_SAY.to_string()));
        Ok(resp)
    }
}

#[tokio::test]
async fn scenario_915_reply2_plus_body_flag_on_last_post_say() {
    let buf = install_capture();
    let mock = Arc::new(Reply2PlusBodyMock);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9182", "R7MARK 2 本返信して本文も出して");

    let say_completed = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R7_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        say_completed,
        "本文 say＋🏁 が観測できない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let say_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R7_SAY))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("R7_SAY say の message id");
    let completed_on_say = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == say_mid
        })
        .count();
    assert_eq!(
        completed_on_say,
        1,
        "🏁 が到着順で最後の投稿（say）に 1 件で付かない（§13.3.6 row 7）: {:?}",
        captured(&buf)
    );
    // reply には 🏁 は付かない（最後の投稿は say）。
    let reply_id_2 = captured(&buf)
        .iter()
        .find(|c| c.kind == "reply" && c.body.contains(R7_2))
        .map(|c| c.reply_id.clone())
        .filter(|m| !m.is_empty());
    if let Some(rid) = reply_id_2 {
        let on_reply = captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == rid
            })
            .count();
        assert_eq!(
            on_reply,
            0,
            "🏁 が reply に誤付与（最後の投稿は say のはず）: {:?}",
            captured(&buf)
        );
    }
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 16【反復上限（max_iterations）到達 → 最後に配送した投稿に 1】: 常に
// 「本文＋CONTINUE」を返し続けるターンは depth0 の上限（process.rs:1583・30）で打ち切られる。
// 打ち切られた最終生成の最後の投稿（＝最後に配送した say）にだけ 🏁 1・他 0・総数 1。
// 現 tip は say 配送ごとに 🏁 を付けるため全 say に付く → **赤**。上限は既存ハードコードを実走
// （dry-run なので高速・test-only の上限注入は作らない・統括裁定）。
// ---------------------------------------------------------------------------
const ML_PREFIX: &str = "mlsay-iteration-";

struct AlwaysContinueMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for AlwaysContinueMock {
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
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 常に「本文＋末尾 CONTINUE」で継続 → 上限まで回る。各本文は一意（buffer 順で最後を特定）。
        Ok(text_response(&format!("{ML_PREFIX}{n:03}\nCONTINUE")))
    }
}

#[tokio::test]
async fn scenario_915_max_iterations_flag_only_on_last_delivered_say() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(AlwaysContinueMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9190", "MLMARK ずっと続けて（上限まで）");

    // 上限打ち切りまで走る。LLM 呼び出しが 30 回に達する（depth0 上限）まで待つ。
    let looped = {
        let mock = mock.clone();
        wait_until(move || mock.calls.load(Ordering::SeqCst) >= 30).await
    };
    assert!(
        looped,
        "上限まで回らない（LLM calls={}）",
        mock.calls.load(Ordering::SeqCst)
    );
    // 打ち切り後の決着（ended）猶予。
    tokio::time::sleep(Duration::from_millis(800)).await;

    // このターンの mlsay say を buffer 順（配送順）で集める。最後の 1 つが「最後に配送した投稿」。
    let ml_says: Vec<String> = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(ML_PREFIX))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .collect();
    assert!(
        ml_says.len() >= 2,
        "上限ケースの say が 2 通以上出ていない（実測 {}）: {:?}",
        ml_says.len(),
        captured(&buf)
    );
    let last_say = ml_says.last().unwrap().clone();

    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 🏁 は最後に配送した say に 1・総数 1（§13.3.6 row 16）。現 tip は全 say に付く → 赤。
    assert_eq!(
        completed_on(&last_say),
        1,
        "🏁 が最後に配送した say に 1 件で付かない（§13.3.6 row 16）: {:?}",
        captured(&buf)
    );
    let total: usize = ml_says.iter().map(|id| completed_on(id)).sum();
    assert_eq!(
        total,
        1,
        "上限ターンの 🏁 総数が 1 でない（全 say に付いている）: mlsays={}, 🏁総数={}",
        ml_says.len(),
        total
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.6 row 10【本文＋query ツール（holding・spawn）→ 宣言 0 ／ resume 完了報告 → 1】:
// **実物の execute_shell**（echo・即時決着＝date 相当）を呼ぶターン。execute_shell は inline 集合に
// 無いので背景 subtask 化され（#152/#671）、dispatch 直後の継続で宣言（holding）say を投稿する。
// subtask 決着（実 echo → settle）後の resume ターンで完了報告 say を投稿。🏁 は宣言 say には付かず
// （進行中）、resume 報告 say に 1・総数 1。統括裁定: 照会クラス常時 detach なので「date 単独」も実機は
// この execute_shell→spawned→resume 経路。現 tip は say 配送ごとに 🏁 → 宣言にも付く → **赤**（総数 2）。
// 偽ツールは作らず、echo のみ許可した実 shell 設定で実走する。
// ---------------------------------------------------------------------------
const SP_ECHO: &str = "specho-shell-stdout-即時";
const SP_DECL: &str = "spdecl-調べてるね（宣言・holding）";
const SP_REPORT: &str = "spreport-結果はこうだった（完了報告投稿）";

struct ShellSpawnResumeMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for ShellSpawnResumeMock {
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
        // (2) dispatch 直後の継続（合成 "spawned" 結果＝tool role）→ 宣言 say（holding）でターンを閉じる。
        if has_tool_role(&request) {
            return Ok(text_response(SP_DECL));
        }
        // (1) 初回（tool role 無し・1 回だけ）→ 実 execute_shell（echo）を呼ぶ＝背景 subtask 化。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [SP_ECHO] }),
            ));
        }
        // (3) subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 完了報告 say。
        Ok(text_response(SP_REPORT))
    }
}

#[tokio::test]
async fn scenario_915_spawned_declaration_no_flag_resume_report_gets_flag() {
    let buf = install_capture();
    let mock = Arc::new(ShellSpawnResumeMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    // echo を実走させるため shell を有効化（tools_config は Arc<RwLock> 共有で runtime に即反映）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9200", "SPSHELLMARK 調べて終わったら教えて");

    // 宣言 say（SP_DECL）と resume 完了報告 say（SP_REPORT）が両方出るまで待つ。
    let both = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            caps.iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SP_DECL))
                && caps
                    .iter()
                    .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(SP_REPORT))
        })
        .await
    };
    assert!(
        both,
        "宣言 say と resume 完了報告 say が揃わない（subtask/resume 未達）: {:?}",
        captured(&buf)
    );
    // 決着後の 🏁 付与猶予。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let decl_mid = mid_of(SP_DECL).expect("宣言 say の message id");
    let report_mid = mid_of(SP_REPORT).expect("完了報告 say の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };

    // 宣言 say には 🏁 は付かない（進行中＝subtask 未決着）。現 tip は付く → 赤。
    assert_eq!(
        completed_on(&decl_mid),
        0,
        "🏁 が spawned 宣言 say に誤付与（進行中は付けない・§13.3.6）: {:?}",
        captured(&buf)
    );
    // resume 完了報告 say には 🏁 1。
    assert_eq!(
        completed_on(&report_mid),
        1,
        "🏁 が resume 完了報告 say に 1 件で付かない（§13.3.6・§13.3.4）: {:?}",
        captured(&buf)
    );
    // 2 say（宣言・報告）合計で 🏁 は 1（宣言に付く現 tip は総数 2 → 赤）。
    let total = completed_on(&decl_mid) + completed_on(&report_mid);
    assert_eq!(
        total,
        1,
        "spawned→resume の 🏁 総数が 1 でない（宣言に誤付与）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #915 / §13.3.1【別 session の subtask 進行中 → 本ターンの投稿に 🏁 0（エージェント単位 idle）】:
// チャンネル A（session A）で subtask を起こし**保留**（決着させない）。その状態でチャンネル B
// （session B）へ通常メッセージを送り say を投稿。エージェントに未決着 subtask があるので B の say は
// idle でない＝🏁 0。現 tip は say 配送ごと（session を見ない）に 🏁 → B の say に付く → **赤**。
// §13.3.1 案E（agent 単位）確定・§13.3.5 は agent-scope 集計を要ビルド検証と明記。
// 専用チャンネル 602（spawner）/603（plain）で他テストと分離。
// ---------------------------------------------------------------------------
const CHANNEL_XA: &str = "602";
const CHANNEL_XB: &str = "603";
const M_XSPAWN: &str = "XSPAWNMARK";
const M_XPLAIN: &str = "XSPLAINMARK";
const XS_DECL: &str = "xsdecl-Aの宣言投稿";
const XS_PLAIN: &str = "xsplain-Bの通常発言";

struct ShellSleepCrossMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for ShellSleepCrossMock {
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
        // (D) チャンネル B の通常ターン: say を返す。
        if text.contains(M_XPLAIN) {
            return Ok(text_response(XS_PLAIN));
        }
        // (B) チャンネル A の execute_shell 後（tool_result 有り）: 宣言 say（holding）。
        if has_tool_role(&request) {
            return Ok(text_response(XS_DECL));
        }
        // (A) チャンネル A の初回: 実 execute_shell（sleep＝遅延決着）で背景 subtask を起こし保留。
        // emitted で 1 回だけ（resume ターンで再発行して無限ループしないため）。
        if text.contains(M_XSPAWN) && !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "sleep", "args": ["8"] }),
            ));
        }
        Ok(text_response("xsfiller"))
    }
}

#[tokio::test]
async fn scenario_915_other_session_subtask_in_progress_no_flag() {
    let buf = install_capture();
    let mock = Arc::new(ShellSleepCrossMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    // sleep を実走させるため shell を有効化（echo・sleep を許可）。
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    // チャンネル A（session A）: execute_shell(sleep) で subtask を起こして保留。
    let fixture_a = Fixture::new();
    let _client_a = wire_instance_on_channel(&core, &fixture_a, CHANNEL_XA).await;
    // チャンネル B（session B）: 通常ターン。
    let fixture_b = Fixture::new();
    let _client_b = wire_instance_on_channel(&core, &fixture_b, CHANNEL_XB).await;

    fixture_a.append_message_ch("9210", CHANNEL_XA, &format!("{M_XSPAWN} 調べて"));

    // A の宣言 say が出る＝subtask を起こして走行中（保留中）になったことの proxy。
    let a_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL_XA && c.body.contains(XS_DECL))
        })
        .await
    };
    assert!(
        a_ready,
        "A の宣言 say が出ない（subtask 未起動）: {:?}",
        captured(&buf)
    );

    // subtask 保留中に B へ通常メッセージ → say を投稿。
    fixture_b.append_message_ch("9211", CHANNEL_XB, &format!("{M_XPLAIN} 2 足す 2 は?"));
    let b_ready = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL_XB && c.body.contains(XS_PLAIN))
        })
        .await
    };
    assert!(b_ready, "B の通常 say が出ない: {:?}", captured(&buf));
    // false-red 防止（統括指摘）: B の 🏁 判定は B の activity ended（B の say 直後）で行われる。
    // その時点で A の subtask（sleep 8）が走行中であることを明示確認する。走行中でなければ sleep 窓を
    // 過ぎており、B が正しく 🏁 を得た（＝テスト前提崩れ）ので、assert ではなくこの確認で弾く。
    assert!(
        core.state
            .subtask_registries
            .has_running_for_agent(AGENT_ID),
        "B 判定時点で A の subtask が走行中でない（sleep 窓を過ぎた・テスト前提崩れ）: {:?}",
        captured(&buf)
    );
    // 決着（🏁 付与）の猶予。この間 subtask は sleep 8 で保留のまま。
    tokio::time::sleep(Duration::from_millis(600)).await;

    let b_mid = captured(&buf)
        .iter()
        .find(|c| c.kind == "say" && c.channel == CHANNEL_XB && c.body.contains(XS_PLAIN))
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
        .expect("B の say の message id");
    let completed_on_b = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == b_mid
        })
        .count();

    // エージェントに未決着 subtask（session A）があるので、B の say には 🏁 を付けない（§13.3.1 案E）。
    // 現 tip は say 配送ごとに付ける（session を見ない）ため B に付く → 赤。
    assert_eq!(
        completed_on_b,
        0,
        "🏁 が別 session の subtask 進行中に B の say へ誤付与（agent 単位 idle・§13.3.1）: {:?}",
        captured(&buf)
    );
    // sleep 3 は自然終了するので後片付け不要（B の判定はその窓の中で確定済み）。
}

// ---------------------------------------------------------------------------
// #915 typing【最終投稿後の入力中停止】: 発話ターンで activity ended を受けたら typing keepalive
// が止まり、失効間隔（8 秒）を跨いでも typing の再送出が 0 であることを観測境界（dry-run
// kind="typing" のキャプチャ増分）で確定する。単一スレッド実行なので、この待機中に typing が
// 増えるのはこのターンの keepalive だけ（他テストは走らない）。
// ---------------------------------------------------------------------------
const TY_SAY: &str = "tysay-入力中停止確認の本文";

struct SingleSayThenIdleMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SingleSayThenIdleMock {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(text_response(TY_SAY))
    }
}

#[tokio::test]
async fn scenario_915_typing_stops_after_turn_end() {
    let buf = install_capture();
    let mock = Arc::new(SingleSayThenIdleMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    // typing は capture に scope key（message）を持たず、CI は並列・BUFFER 共有なので、他テストが
    // 使わない専用チャンネル（CHANNEL_TY）へ束ねて typing を隔離する。
    let _client = wire_instance_on_channel(&core, &fixture, CHANNEL_TY).await;

    fixture.append_message_ch(
        "9154",
        CHANNEL_TY,
        &format!("{M_SAY} 入力中がターン後に止まるか"),
    );

    // 最終 say（TY_SAY）の own id に 🏁 が付く＝activity ended を受けた（typing も ended で停止）まで待つ。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            let mid = captured(&buf)
                .iter()
                .find(|c| c.kind == "say" && c.channel == CHANNEL_TY && c.body.contains(TY_SAY))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty());
            match mid {
                Some(mid) => captured(&buf).iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.message == mid
                }),
                None => false,
            }
        })
        .await
    };
    assert!(
        done,
        "最終 say＋🏁（＝ended 到達）が観測できない: {:?}",
        captured(&buf)
    );

    // ended 到達後、専用チャンネルの typing キャプチャ数を基準化し、失効間隔
    // （TYPING_REFRESH_INTERVAL=8 秒）を跨いで待つ。このチャンネルはこのターンだけが使うので、
    // 停止していれば増分 0・停止漏れなら keepalive が 8 秒後に再送出して増える（並列でも堅牢）。
    let before = count_kind_on_channel(&buf, "typing", CHANNEL_TY);
    tokio::time::sleep(Duration::from_millis(9000)).await;
    let after = count_kind_on_channel(&buf, "typing", CHANNEL_TY);
    assert_eq!(
        after, before,
        "activity ended 後に typing が再送出された（入力中が止まらない）: before={before} after={after}"
    );
}

// ---------------------------------------------------------------------------
// §13.1 g【reaction のみ → #6 と同じ】: reaction のみ（say 0）のターン → reaction 配送・
// 🤐 発端に付けない（発話がある）。現 tip: 最終本文空を沈黙とみなし 🤐 → 赤（#900 の reaction 版）。
// §13 #6 の 🤐 契約を reaction で固定。
// ---------------------------------------------------------------------------
const M_REACT_ONLY: &str = "REACTONLYMARK";
const REACT_EMOJI: &str = "🎉";

struct ReactionOnlyMock {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReactionOnlyMock {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if !has_tool_role(&request) && request_text(&request).contains(M_REACT_ONLY) {
            return Ok(tool_call_response(
                "reaction",
                serde_json::json!({"event": "e1", "emoji": REACT_EMOJI}),
            ));
        }
        Ok(text_response("NO_REPLY"))
    }
}

#[tokio::test]
async fn audit_s13_1g_reaction_only_turn_gets_no_muted_reaction() {
    use std::sync::atomic::Ordering;
    let buf = install_capture();
    let mock = Arc::new(ReactionOnlyMock {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message(
        "1309",
        &format!("{M_REACT_ONLY} これにリアクションだけして"),
    );

    // agent の reaction（kind=reaction）が発端 1309 に出るまで待つ。
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.message == "1309")
        })
        .await
    };
    assert!(delivered, "reaction が配送されない: {:?}", captured(&buf));

    // 決着の猶予（🤐 判定は決着時）。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 🤐 なし: 発話（reaction）があったターンなので発端 1309 に 🤐 は付かない（現 tip は付く → 赤）。
    let muted = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "1309")
        .count();
    assert_eq!(
        muted,
        0,
        "reaction があったターンに 🤐 が誤発火（#900・§13 #6 の reaction 版）: {:?}",
        captured(&buf)
    );

    // §13.2: reaction のみのターンは自分の「投稿」が無い（reaction は発話 op だが say/reply の
    // ような本文投稿ではない）ので 🏁 は付かない（発端 1309 への誤付与を総数 0 で pin）。
    let completed_on_1309 = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "1309"
        })
        .count();
    assert_eq!(
        completed_on_1309,
        0,
        "reaction のみのターンに 🏁 が誤発火（§13.2）: {:?}",
        captured(&buf)
    );
    assert!(
        mock.calls.load(Ordering::SeqCst) >= 1,
        "reaction ターンの LLM 呼び出しが走らない"
    );
}

// ---------------------------------------------------------------------------
// #916 §13.4.1【holding 本文（本文＋execute_shell）が V3 で配送・保存され、宣言に 🏁 0・
// resume 報告に 🏁 1・ack 後 NO_REPLY で追加投稿 0/🤐 0】:
//   1 生成目 = content(宣言)＋execute_shell(echo) の holding。現 tip は holding 本文を
//   gateway へ配送せず on_tool_call で保存だけする（skill_engine.rs:1109-1110「holding は
//   従来経路」・:850-856 で content は on_tool_call のみ）→ 宣言 say が dry-run に出ない → 赤。
//   spawn ack（spawned 合成結果＝tool role）は NO_REPLY でターンを閉じる（追加投稿 0）。
//   subtask（echo）決着 → resume 完了報告 say。宣言に 🏁 なし・報告に 🏁 1。
// 観測境界: dry-run kind="say"（配送）・memory_sessions speech（保存）・system_reaction（🏁/🤐）。
// ---------------------------------------------------------------------------
const M_916: &str = "HOLD916MARK";
const H916_DECL: &str = "h916decl-調べるね（holding 宣言・execute_shell と同一生成）";
const H916_REPORT: &str = "h916report-終わったよ（resume 完了報告）";
const H916_ECHO: &str = "h916-echo-即時stdout";

struct HoldingShellMock {
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for HoldingShellMock {
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
        // spawn ターンの ack（合成 "spawned" 結果＝tool role）→ NO_REPLY で追加投稿せず閉じる。
        if has_tool_role(&request) {
            return Ok(text_response("NO_REPLY"));
        }
        // 初回（tool role 無し・1 回だけ）→ 本文（宣言）＋execute_shell を同一生成で（holding）。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(shell_with_content_response(H916_DECL, "echo", &[H916_ECHO]));
        }
        // subtask 決着後の resume ターン（tool role 無し・2 回目以降）→ 完了報告 say。
        Ok(text_response(H916_REPORT))
    }
}

#[tokio::test]
async fn scenario_916_holding_body_delivered_and_saved() {
    let buf = install_capture();
    let mock = Arc::new(HoldingShellMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("9220", &format!("{M_916} sleep 60 して終わったら教えて"));

    // §13.4.1 手順 2/4: 宣言 holding say（H916_DECL）と resume 報告 say（H916_REPORT）が両方出るまで待つ。
    // 現 tip は holding 本文が V3 で未配送のため H916_DECL の say が出ず、この wait は false → 赤。
    let both = {
        let buf = buf.clone();
        wait_until(move || {
            let caps = captured(&buf);
            caps.iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(H916_DECL))
                && caps.iter().any(|c| {
                    c.kind == "say" && c.channel == CHANNEL && c.body.contains(H916_REPORT)
                })
        })
        .await
    };
    assert!(
        both,
        "宣言 holding say（本文＋execute_shell）と resume 報告 say が揃わない（現 tip: holding 本文が V3 で未配送・§13.4.1 手順 2/5）: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 配送 count: 宣言 say 1・報告 say 1（§13.4.1 手順 2/4）。
    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .count()
    };
    assert_eq!(
        say_count(H916_DECL),
        1,
        "宣言 holding 本文が Discord に 1 件配送されていない（現 tip 0＝V3 未配送）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(H916_REPORT),
        1,
        "resume 完了報告が 1 件配送されていない: {:?}",
        captured(&buf)
    );

    // 否定側 1: ack の NO_REPLY は配送しない（追加投稿 0・§13.4.1 手順 3）。
    let noreply_say = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains("NO_REPLY"))
        .count();
    assert_eq!(
        noreply_say,
        0,
        "ack の NO_REPLY が投稿された（追加投稿・§13.4.1 手順 3）: {:?}",
        captured(&buf)
    );

    // 否定側 2: 発話ありターンなので発端 9220 に 🤐 は付かない（§13.4.1 手順 3）。
    let muted_on_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "9220")
        .count();
    assert_eq!(
        muted_on_origin,
        0,
        "発話ありターンに 🤐 が誤発火（§13.4.1 手順 3・追加投稿 0/🤐 0）: {:?}",
        captured(&buf)
    );

    // 🏁: 宣言 say に 0（進行中＝subtask 未決着）・報告 say に 1（§13.4.1 手順 2/4）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let decl_mid = mid_of(H916_DECL).expect("宣言 say の message id");
    let report_mid = mid_of(H916_REPORT).expect("報告 say の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&decl_mid),
        0,
        "🏁 が spawned 宣言 say に誤付与（進行中は付けない・§13.4.1 手順 2）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&report_mid),
        1,
        "🏁 が resume 完了報告 say に 1 件で付かない（§13.4.1 手順 4）: {:?}",
        captured(&buf)
    );

    // 保存＝配送の一致（§13.4.1 手順 5）: memory_sessions の自 speech は宣言＋報告の 2 行のみ。
    // 現 tip は宣言 holding が保存だけされ配送されない（保存 2・配送 1）＝Discord に出ていない文が
    // 保存されている状態。配送（say 2）と保存（2 行）の一致で「出ていない文が保存されていない」を固定。
    let own_speech_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        own_speech_rows, 2,
        "自 speech 保存行が宣言＋報告の 2 行でない（§13.4.1 手順 5）: {own_speech_rows}"
    );
    let delivered_says = say_count(H916_DECL) + say_count(H916_REPORT);
    assert_eq!(
        delivered_says as i64, own_speech_rows,
        "配送数（say {delivered_says}）と保存数（speech {own_speech_rows}）が一致しない（Discord に出ていない文が保存されている・§13.4.1 手順 5）: {:?}",
        captured(&buf)
    );
}

// ---------------------------------------------------------------------------
// #918 §13.4.2【resume ターンで 本文＋CONTINUE→本文 → 配送 2・保存 2・🏁 は 2 件目のみ・
// CONTINUE 残留 0】:
//   spawn ターンで宣言（別生成）→ subtask（echo）決着 → resume ターンが本文1＋末尾CONTINUE →
//   本文2 で 2 分割。resume の途中発話（本文1）が配送・保存され、最終（本文2）に 🏁 1、途中に 🏁 0、
//   どの say にも "CONTINUE" が出ない。
//   注（実装調査）: base tip 9dc50f35(#917) が completion.rs:194 に on_continuation_speech を配線済み。
//   本テストは #918 の現状（赤 or 緑）を実証する探り。緑なら「#917 で解消済み」を確定させる回帰。
// ---------------------------------------------------------------------------
const SP918_DECL: &str = "sp918decl-調べるね（spawn 宣言）";
const R918_1: &str = "r918-報告その1（resume 途中発話）";
const R918_2: &str = "r918-報告その2（resume 最終）";
const H918_ECHO: &str = "h918-echo-即時stdout";

struct ResumeContinueMock {
    emitted: std::sync::atomic::AtomicBool,
    resume_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ResumeContinueMock {
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
        // spawn ターンの ack（合成 "spawned" 結果＝tool role）→ 宣言 say（別生成・配送される）。
        if has_tool_role(&request) {
            return Ok(text_response(SP918_DECL));
        }
        // 初回（tool role 無し・1 回だけ）→ execute_shell(echo) で背景 subtask 化。
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(tool_call_response(
                "execute_shell",
                serde_json::json!({ "command": "echo", "args": [H918_ECHO] }),
            ));
        }
        // subtask 決着後の resume ターン（tool role 無し・emitted 済み）: 本文1＋CONTINUE → 本文2。
        let n = self
            .resume_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match n {
            0 => Ok(text_response(&format!("{R918_1}\nCONTINUE"))),
            _ => Ok(text_response(R918_2)),
        }
    }
}

#[tokio::test]
async fn scenario_918_resume_turn_continue_split_delivers_both() {
    let buf = install_capture();
    let mock = Arc::new(ResumeContinueMock {
        emitted: std::sync::atomic::AtomicBool::new(false),
        resume_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message(
        "9230",
        "SP918MARK sleep 5 して終わったら 2 回に分けて報告して",
    );

    // resume の 2 件目（R918_2）まで出るのを待つ。#918 が未修正なら R918_1 が配送されず、
    // 下の count assert が赤になる。
    let done = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(R918_2))
        })
        .await
    };
    assert!(
        done,
        "resume 最終報告（R918_2）が出ない: {:?}",
        captured(&buf)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    let say_count = |body: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .count()
    };
    // 手順 2: spawn 宣言が 1 件配送（別生成・#916 の holding とは別経路）。
    assert_eq!(
        say_count(SP918_DECL),
        1,
        "spawn 宣言が 1 件配送されない（§13.4.2 手順 2）: {:?}",
        captured(&buf)
    );
    // 配送 2: resume 途中発話（R918_1）＋最終（R918_2）が各 1 件。現 tip で R918_1 が落ちるなら赤。
    assert_eq!(
        say_count(R918_1),
        1,
        "resume 途中発話（本文1）が配送されない（§13.4.2 手順 3・#918）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        say_count(R918_2),
        1,
        "resume 最終（本文2）が配送されない: {:?}",
        captured(&buf)
    );

    // CONTINUE 残留 0: どの say にも "CONTINUE" が出ない（§13.4.2 手順 3）。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say"
                && c.channel == CHANNEL
                && (c.body.contains(R918_1) || c.body.contains(R918_2)))
            .all(|c| !c.body.contains("CONTINUE")),
        "resume の say に CONTINUE が残留（§13.4.2 手順 3/5）: {:?}",
        captured(&buf)
    );

    // 🏁: 途中（R918_1）に 0・最終（R918_2）に 1（§13.4.2 手順 3）。
    let mid_of = |body: &str| -> Option<String> {
        captured(&buf)
            .iter()
            .find(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(body))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
    };
    let r1_mid = mid_of(R918_1).expect("R918_1 の message id");
    let r2_mid = mid_of(R918_2).expect("R918_2 の message id");
    let completed_on = |id: &str| -> usize {
        captured(&buf)
            .iter()
            .filter(|c| {
                c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == id
            })
            .count()
    };
    assert_eq!(
        completed_on(&r1_mid),
        0,
        "🏁 が resume 途中発話に誤付与（§13.4.2 手順 3・1 件目に 🏁 なし）: {:?}",
        captured(&buf)
    );
    assert_eq!(
        completed_on(&r2_mid),
        1,
        "🏁 が resume 最終に 1 件で付かない（§13.4.2 手順 3）: {:?}",
        captured(&buf)
    );

    // 保存: 宣言＋報告 2 行の計 3 行（§13.4.2 手順 4）。
    let own_speech_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        own_speech_rows, 3,
        "自 speech 保存行が宣言 1＋報告 2 の 3 行でない（§13.4.2 手順 4）: {own_speech_rows}"
    );
}
