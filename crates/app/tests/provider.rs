//! 本物の推論の口（`HttpSseEngine`）を、**ローカルの自己署名 TLS サーバ**に対して検証する（目標2）。
//!
//! TLS は `reqwest` に終端させる（自前で張らない）。鍵は要らない（偽サーバは鍵を見ない）。守るのは:
//!   1. 応答をストリームで受け、断片ごとに `chunk()` を叩き、本文を say として返す（**本物の TLS 経路**）。
//!   2. **アイドルの上限が本物の TLS 経路でも効く**——止まった生成は切れる（§05）。
//!   3. 断片が流れている限り、総時間が上限を超えても切られない（§05）。
//!   4. プロバイダの `tool_use` を `ToolCallSpec` に写し、core の道具の経路に繋ぐ（目標3）。
//!      入力（`input_json_delta`）を断片跨ぎで積む。ツールを呼ぶと同じターンで結果が還り、再推論する。
//!
//! 実時間で回す（実ソケット + start_paused は相性が悪い）。上限は短く（300ms）。

use opencrab_app::{
    AnthropicProvider, ChatGptProvider, ChatProvider, Host, HttpSseEngine, OpenAiProvider,
};
use opencrab_port::{
    Block, ChunkSink, Context, EffectSpec, Engine, EngineError, InferOutput, Message, MsgRole,
    Property, Role, Standing, SubjectKind, ToolDef,
};
use opencrab_social_runtime::{Config, Incoming, Policy};
use opencrab_store::Store;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

// ---- SSE イベントの部品 ----

fn text_delta(t: &str) -> Value {
    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text": t}})
}
fn message_stop() -> Value {
    json!({"type":"message_stop"})
}
fn end_turn() -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}})
}
/// ツール呼び出しブロックの開始（name のみ・input は delta で来る）。
fn tool_start(index: u64, name: &str) -> Value {
    json!({"type":"content_block_start","index":index,
           "content_block":{"type":"tool_use","id":"toolu_x","name":name,"input":{}}})
}
/// ツール入力（JSON）の断片。断片跨ぎで積まれることを確かめるため分割して送れる。
fn tool_input(index: u64, partial: &str) -> Value {
    json!({"type":"content_block_delta","index":index,
           "delta":{"type":"input_json_delta","partial_json":partial}})
}
fn stop_tool_use() -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}})
}

// ---- 自己署名 TLS の偽 SSE サーバ ----

/// 1 接続分の応答台本: (間隔, data の JSON) の列と、送り終えたあと閉じるか。
/// `close_after=false` なら閉じずに握ったまま止まる（ストールを作る）。
#[derive(Clone)]
struct Script {
    events: Vec<(Duration, Value)>,
    close_after: bool,
    status: u16,
    body: String,
}

fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // 偽サーバ側の rustls。reqwest は自分の provider を持つので競合しない。
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// rcgen で 127.0.0.1 の自己署名証明書を作り、TLS 受理器と DER（クライアントが信頼する根）を返す。
fn self_signed() -> (TlsAcceptor, Vec<u8>) {
    ensure_crypto_provider();
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let cert_der: CertificateDer<'static> = ck.cert.der().clone();
    let der_bytes = cert_der.as_ref().to_vec();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    (TlsAcceptor::from(Arc::new(cfg)), der_bytes)
}

/// 自己署名の根を信頼する reqwest クライアント（本物の TLS 検証を通す。danger フラグは使わない）。
fn client_trusting(cert_der: &[u8]) -> reqwest::Client {
    let cert = reqwest::Certificate::from_der(cert_der).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .build()
        .unwrap()
}

/// 1 接続を SSE で応対する（平文でも TLS でも同じ。ストリームは AsyncRead+AsyncWrite）。
async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(mut sock: S, script: Script) {
    // 要求を軽く読み流す（本文は小さいので届く）。ブロックし続けないよう期限つき。
    let mut rb = [0u8; 8192];
    let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut rb)).await;
    let head = if script.status == 200 {
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            .to_string()
    } else {
        format!(
            "HTTP/1.1 {} Test Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            script.status,
            script.body.len()
        )
    };
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if script.status != 200 {
        let _ = sock.write_all(script.body.as_bytes()).await;
        let _ = sock.flush().await;
        return;
    }
    let _ = sock.flush().await;
    for (gap, data) in &script.events {
        tokio::time::sleep(*gap).await;
        let frame = format!("event: x\ndata: {}\n\n", data);
        if sock.write_all(frame.as_bytes()).await.is_err() {
            return;
        }
        let _ = sock.flush().await;
    }
    if !script.close_after {
        // ストール: 送らず・閉じず、接続を握ったまま止まる。
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    // close_after: TLS close_notify を送ってから閉じ、クライアントに正常な EOF を見せる。
    // drop だけだと rustls は truncated TLS と判定し、EOF 判定前に decode error になる。
    let _ = sock.shutdown().await;
}

/// TLS の偽サーバを立て、(base_url, 信頼すべき証明書 DER) を返す。
/// `scripts` は接続ごとに前から 1 つ消費する（ツールループのように infer が複数回来る場合に使う）。
/// 1 つしか無いときは、その台本を全接続で使い回す。
fn spawn_fake_tls(scripts: Vec<Script>) -> (String, Vec<u8>) {
    let (acceptor, cert_der) = self_signed();
    let queue: Arc<Mutex<VecDeque<Script>>> = Arc::new(Mutex::new(scripts.into_iter().collect()));
    // bind を同期的に済ませてポートを確定させてから accept ループを spawn する。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("https://127.0.0.1:{port}");
    tokio::spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            let acceptor = acceptor.clone();
            let script = {
                let mut q = queue.lock().unwrap();
                if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    q.front().cloned().unwrap()
                }
            };
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    serve_one(tls, script).await;
                }
            });
        }
    });
    (base, cert_der)
}

fn one(events: Vec<(Duration, Value)>, close_after: bool) -> Vec<Script> {
    vec![Script {
        events,
        close_after,
        status: 200,
        body: String::new(),
    }]
}

async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

fn short_idle_cfg() -> Config {
    Config {
        idle_cap: Duration::from_millis(300),
        ..Config::default()
    }
}

async fn infer_anthropic_script(
    events: Vec<(Duration, Value)>,
) -> Result<InferOutput, EngineError> {
    let (base, cert) = spawn_fake_tls(one(events, true));
    let engine = HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    );
    let (sink, _rx) = ChunkSink::channel();
    engine.infer(&Context::default(), &sink).await
}

fn engine_error(result: Result<InferOutput, EngineError>) -> String {
    match result {
        Err(EngineError(message)) => message,
        Ok(_) => panic!("expected EngineError"),
    }
}

/// 発火する場を 1 つ用意する（Direct 即応・既定はエージェント）。(place, agent, human) を返す。
fn firing_place(host: &Host) -> (i64, i64, i64) {
    let a = host
        .sys
        .create_subject(SubjectKind::Agent, "A", "A", Standing::Trusted);
    let human = host
        .sys
        .create_subject(SubjectKind::Human, "H", "H", Standing::Owner);
    let p = host.sys.create_place(
        None,
        None,
        &Policy::immediate_on(&[Property::Direct]).with_default(a),
        None,
    );
    host.sys.join(p, a, Role::Participant);
    host.sys.join(p, human, Role::Participant);
    (p, a, human)
}

// 1) 応答をストリームで受け、断片ごとに chunk() を叩き、本文を say として返す（本物の TLS 経路）。
#[tokio::test(flavor = "current_thread")]
async fn provider_streams_text_over_tls_and_feeds_chunks() {
    let (base, cert) = spawn_fake_tls(one(
        vec![
            (Duration::from_millis(5), text_delta("見")),
            (Duration::from_millis(5), text_delta("たよ")),
            (Duration::from_millis(5), end_turn()),
            (Duration::from_millis(5), message_stop()),
        ],
        true,
    ));
    let engine = HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("claude-x", "", 64)),
        client_trusting(&cert),
    );
    let (sink, mut rx) = ChunkSink::channel();
    let ctx = Context {
        rendered: "[1] s1: これ見た？".to_string(),
        ..Context::default()
    };
    let out = engine.infer(&ctx, &sink).await.expect("infer ok");

    assert!(out.done, "end_turn/message_stop で終わる");
    assert_eq!(out.effects.len(), 1, "本文を 1 つの say にする");
    assert_eq!(
        out.effects[0].content.text.as_deref(),
        Some("見たよ"),
        "断片を積んだ本文が say になる"
    );
    assert!(out.tool_calls.is_empty(), "道具呼び出しは無い");

    drop(sink);
    let mut count = 0;
    while rx.recv().await.is_some() {
        count += 1;
    }
    assert!(count >= 2, "SSE の断片ごとに chunk() が叩かれる: {count}");
}

// #13: HTTP 200 でも明示終端を見ない EOF は失敗。部分本文・途中 tool call も成功として救済しない。
#[tokio::test(flavor = "current_thread")]
async fn provider_eof_without_terminal_event_fails_for_empty_text_and_tool_streams() {
    let cases = [
        ("empty", vec![]),
        (
            "partial text",
            vec![(Duration::from_millis(1), text_delta("partial"))],
        ),
        (
            "partial tool",
            vec![
                (Duration::from_millis(1), tool_start(0, "core-child-list")),
                (Duration::from_millis(1), tool_input(0, "{}")),
            ],
        ),
    ];
    for (label, events) in cases {
        let message = engine_error(infer_anthropic_script(events).await);
        assert_eq!(
            message, "provider stream ended before terminal event",
            "{label}"
        );
    }
}

// #13: 200 応答本文内の明示 error は EOF や空応答へ化けず、その本文で EngineError になる。
#[tokio::test(flavor = "current_thread")]
async fn provider_explicit_error_event_becomes_engine_error() {
    let message = engine_error(
        infer_anthropic_script(vec![(
            Duration::from_millis(1),
            json!({"type":"error","error":{"message":"sentinel provider error"}}),
        )])
        .await,
    );
    assert_eq!(message, "provider stream error: sentinel provider error");
}

// #13: HTTP error の理由を core が捨てず、意味的処理より後の単一記録経路で failure_detail に残す。
#[tokio::test(flavor = "current_thread")]
async fn provider_http_error_reason_reaches_turn_record() {
    let (base, cert) = spawn_fake_tls(vec![Script {
        events: vec![],
        close_after: true,
        status: 503,
        body: "recorded provider sentinel".into(),
    }]);
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    ));
    let store = Store::new_in_memory().unwrap();
    store.register_model_context_window("m", 200_000).unwrap();
    let host = Host::boot_with(store, engine, short_idle_cfg());
    let (p, _a, human) = firing_place(&host);

    host.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    let sys = host.sys.clone();
    assert!(
        wait_until(Duration::from_secs(5), || !sys
            .store()
            .turn_records(p)
            .unwrap()
            .is_empty())
        .await
    );

    let records = host.sys.store().turn_records(p).unwrap();
    assert_eq!(records.len(), 1, "retry / fallback しない");
    assert_eq!(records[0].end_reason, "failed");
    assert_eq!(
        records[0].failure_detail.as_deref(),
        Some("provider http status 503: recorded provider sentinel")
    );
    assert_eq!(host.sys.store().latest_seq(p).unwrap(), 1);
    let contexts = host.sys.store().context_records(records[0].id).unwrap();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].iteration, records[0].iterations);
}

// ---- OpenAI 形式（同じ転送・同じアイドル上限。線の組み方だけ別）----

fn oai_text(t: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":t},"finish_reason":null}]})
}
fn oai_stop() -> Value {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
}
/// この環境の橋の形: id+name+**完全な** arguments と finish_reason を 1 チャンクにまとめて出す。
fn oai_tool_combined(index: u64, id: &str, name: &str, args: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[
        {"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args}}
    ]},"finish_reason":"tool_calls"}]})
}

// 1o) OpenAI 形式でも本文をストリームで積んで say にする（本物の TLS 経路）。
#[tokio::test(flavor = "current_thread")]
async fn openai_provider_streams_text_over_tls() {
    let (base, cert) = spawn_fake_tls(one(
        vec![
            (Duration::from_millis(5), oai_text("見")),
            (Duration::from_millis(5), oai_text("たよ")),
            (Duration::from_millis(5), oai_stop()),
        ],
        true,
    ));
    let engine = HttpSseEngine::with_client(
        base,
        Box::new(OpenAiProvider::new("m", "", 64)),
        client_trusting(&cert),
    );
    let (sink, _rx) = ChunkSink::channel();
    let ctx = Context {
        rendered: "[1] s1: これ見た？".to_string(),
        ..Context::default()
    };
    let out = engine.infer(&ctx, &sink).await.expect("infer ok");
    assert!(out.done, "finish_reason=stop で終わる");
    assert_eq!(out.effects.len(), 1);
    assert_eq!(out.effects[0].content.text.as_deref(), Some("見たよ"));
    assert!(out.tool_calls.is_empty());
}

// 4o) 1 チャンクに開始＋入力＋終了理由がまとまって来ても、tool_use として組み上がる（seam が Vec ゆえ）。
#[tokio::test(flavor = "current_thread")]
async fn openai_provider_parses_combined_tool_call_chunk() {
    let (base, cert) = spawn_fake_tls(one(
        vec![(
            Duration::from_millis(5),
            oai_tool_combined(0, "call_1", "core-child-list", r#"{"place":7}"#),
        )],
        true,
    ));
    let engine = HttpSseEngine::with_client(
        base,
        Box::new(OpenAiProvider::new("m", "", 64)),
        client_trusting(&cert),
    );
    let (sink, _rx) = ChunkSink::channel();
    let ctx = Context {
        rendered: "x".to_string(),
        ..Context::default()
    };
    let out = engine.infer(&ctx, &sink).await.expect("infer ok");
    assert!(
        !out.done,
        "finish_reason=tool_calls はターン継続（tool_use へ写す・§07）"
    );
    assert_eq!(out.tool_calls.len(), 1);
    assert_eq!(out.tool_calls[0].name, "core-child-list");
    assert_eq!(out.tool_calls[0].id, "call_1");
    assert_eq!(out.tool_calls[0].args, json!({"place": 7}));
    assert!(out.effects.is_empty(), "本文が無ければ say を出さない");
}

// build_body（ネットワーク無し）: 道具宣言と道具の往復を OpenAI の形へ写す。
#[test]
fn openai_build_body_translates_tools_and_history() {
    let provider = OpenAiProvider::new("m", "k", 64);
    let ctx = Context {
        rendered: "hi".to_string(),
        history: vec![
            Message {
                role: MsgRole::Assistant,
                content: vec![Block::ToolUse {
                    id: "c1".into(),
                    name: "t".into(),
                    input: json!({"a": 1}),
                }],
            },
            Message {
                role: MsgRole::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "c1".into(),
                    content: vec![opencrab_port::Part::text("42")],
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDef {
            name: "t".into(),
            description: "d".into(),
            params: json!({"type": "object"}),
        }],
        ..Context::default()
    };
    let body = provider.build_body(&ctx);
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["model"], json!("m"));
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("t"));
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], json!("user")); // rendered
    assert_eq!(msgs[1]["role"], json!("assistant"));
    assert_eq!(msgs[1]["tool_calls"][0]["id"], json!("c1"));
    assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], json!("t"));
    assert_eq!(msgs[2]["role"], json!("tool")); // tool result は別メッセージ
    assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
    assert_eq!(msgs[2]["content"], json!("42"));
}

/// tool_result の画像パート（DESIGN-images §4）を作る（枠書きテキスト + PNG バイト）。
fn tool_result_with_image() -> Message {
    let png = vec![0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 1, 2, 3];
    Message {
        role: MsgRole::User,
        content: vec![Block::ToolResult {
            tool_use_id: "c1".into(),
            content: vec![
                opencrab_port::Part::text("枠書き"),
                opencrab_port::Part::ImageBytes {
                    media_type: "image/png".into(),
                    data: png,
                },
            ],
            is_error: false,
        }],
    }
}

// Anthropic への写像（§4）: tool_result content が text + image（base64 source）の配列になる。
#[test]
fn anthropic_maps_image_tool_result_to_base64_block() {
    let provider = AnthropicProvider::new("m", "k", 64);
    let ctx = Context {
        rendered: "hi".to_string(),
        history: vec![tool_result_with_image()],
        ..Context::default()
    };
    let body = provider.build_body(&ctx);
    let msgs = body["messages"].as_array().unwrap();
    // msgs[0] = rendered、msgs[1] = tool_result を載せた user。
    let tr = &msgs[1]["content"][0];
    assert_eq!(tr["type"], json!("tool_result"));
    let parts = tr["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], json!("text"));
    assert_eq!(parts[1]["type"], json!("image"));
    assert_eq!(parts[1]["source"]["type"], json!("base64"));
    assert_eq!(parts[1]["source"]["media_type"], json!("image/png"));
    assert!(
        parts[1]["source"]["data"]
            .as_str()
            .unwrap()
            .starts_with("iVBORw"),
        "PNG マジックの base64 は iVBORw で始まる: {}",
        parts[1]["source"]["data"]
    );
}

// OpenAI chat への写像（§4）: 画像を含む tool メッセージは content が image_url（data URI）の配列。
#[test]
fn openai_maps_image_tool_result_to_data_uri() {
    let provider = OpenAiProvider::new("m", "k", 64);
    let ctx = Context {
        rendered: "hi".to_string(),
        history: vec![tool_result_with_image()],
        ..Context::default()
    };
    let body = provider.build_body(&ctx);
    let msgs = body["messages"].as_array().unwrap();
    let tool_msg = msgs.iter().find(|m| m["role"] == json!("tool")).unwrap();
    let parts = tool_msg["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], json!("text"));
    assert_eq!(parts[1]["type"], json!("image_url"));
    assert!(
        parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,iVBORw"),
        "data URI（base64）へ写す: {}",
        parts[1]["image_url"]["url"]
    );
}

// 2) 止まった生成は、本物の TLS 経路でもアイドル上限で切れる（§05）。
#[tokio::test(flavor = "current_thread")]
async fn stalled_provider_hits_idle_cap_over_tls() {
    // ヘッダ + 1 断片を送って、その後**止まる**（message_stop を送らない・閉じない）。
    let (base, cert) = spawn_fake_tls(one(
        vec![(Duration::from_millis(5), text_delta("考え中"))],
        false,
    ));
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    ));
    let store = Store::new_in_memory().unwrap();
    // このテストの provider はダミーモデル "m"。予算の物差し（§06）に context_window を登録する
    // （boot_with の seed は既知モデルだけなので、テスト固有の "m" はここで足す）。
    store.register_model_context_window("m", 200_000).unwrap();
    let host = Host::boot_with(store, engine, short_idle_cfg());
    let (p, _a, human) = firing_place(&host);

    host.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    let sys = host.sys.clone();
    assert!(
        wait_until(Duration::from_secs(5), || !sys
            .store()
            .turn_records(p)
            .unwrap()
            .is_empty())
        .await,
        "ターン記録が書かれる（止まっても記録は残る・§05）"
    );
    let recs = host.sys.store().turn_records(p).unwrap();
    assert_eq!(
        recs[0].end_reason, "idle_timeout",
        "本物の TLS 経路でも、止まった生成はアイドル上限で切れる（§05）"
    );
    // 枠が解放されている（別のターンが取れる）。
    assert!(host.sys.store().running_activities().unwrap().is_empty());
}

// 3) 断片が流れている限り、総時間が上限を超えても切られない（本物の TLS 経路・§05）。
#[tokio::test(flavor = "current_thread")]
async fn long_streaming_provider_not_cut_over_tls() {
    // 各間隔 100ms（< 上限 300ms）で 4 断片、総 400ms（> 上限）→ 切られず完了。
    let (base, cert) = spawn_fake_tls(one(
        vec![
            (Duration::from_millis(100), text_delta("A")),
            (Duration::from_millis(100), text_delta("B")),
            (Duration::from_millis(100), text_delta("C")),
            (Duration::from_millis(100), text_delta("D")),
            (Duration::from_millis(10), end_turn()),
            (Duration::from_millis(5), message_stop()),
        ],
        true,
    ));
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    ));
    let store = Store::new_in_memory().unwrap();
    // このテストの provider はダミーモデル "m"。予算の物差し（§06）に context_window を登録する
    // （boot_with の seed は既知モデルだけなので、テスト固有の "m" はここで足す）。
    store.register_model_context_window("m", 200_000).unwrap();
    let host = Host::boot_with(store, engine, short_idle_cfg());
    let (p, _a, human) = firing_place(&host);

    host.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    let sys = host.sys.clone();
    assert!(
        wait_until(Duration::from_secs(5), || sys
            .store()
            .latest_seq(p)
            .unwrap()
            >= 2)
        .await,
        "ターンが完了して発話が確定する"
    );
    let recs = host.sys.store().turn_records(p).unwrap();
    assert_eq!(
        recs[0].end_reason, "done",
        "断片が流れている限り、総時間が上限を超えても切られない（§05）"
    );
    let spoke = host.sys.store().get_event(p, 2).unwrap().unwrap();
    assert_eq!(
        spoke.content.text.as_deref(),
        Some("ABCD"),
        "流れてきた断片を積んだ本文が発話になる"
    );
}

// 4a) プロバイダの tool_use を ToolCallSpec に写す。入力（input_json_delta）を断片跨ぎで積む（目標3）。
#[tokio::test(flavor = "current_thread")]
async fn provider_parses_tool_use_with_split_input() {
    let (base, cert) = spawn_fake_tls(one(
        vec![
            (Duration::from_millis(2), tool_start(0, "core-read-log")),
            // 入力 JSON を 2 つに割って送る（断片跨ぎの積み上げを試す）。
            (Duration::from_millis(2), tool_input(0, "{\"from\":1,")),
            (Duration::from_millis(2), tool_input(0, "\"to\":9}")),
            (Duration::from_millis(2), stop_tool_use()),
            (Duration::from_millis(2), message_stop()),
        ],
        true,
    ));
    let engine = HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    );
    let (sink, _rx) = ChunkSink::channel();
    let out = engine
        .infer(&Context::default(), &sink)
        .await
        .expect("infer ok");

    assert!(
        !out.done,
        "stop_reason=tool_use ならターンは続く（core が道具を呼ぶ・§07）"
    );
    assert!(out.effects.is_empty(), "本文が無いので say は出さない");
    assert_eq!(out.tool_calls.len(), 1, "道具呼び出しが 1 つ");
    assert_eq!(out.tool_calls[0].name, "core-read-log");
    assert_eq!(
        out.tool_calls[0].args,
        json!({"from":1,"to":9}),
        "断片跨ぎで積んだ入力 JSON が組み上がる"
    );
}

// 4b) 端から端: プロバイダが道具を呼ぶ → core が core ツールを実行 → 結果が同じターンに還って再推論
//     → 最終の発話が確定する（目標3の「繋いだら動く」）。core-child-list は ParticipantTool。
#[tokio::test(flavor = "current_thread")]
async fn tool_call_runs_core_tool_and_continues_same_turn() {
    // 接続1: 道具 core-child-list を呼ぶ（stop_reason=tool_use）。接続2: 最終の発話（end_turn）。
    let scripts = vec![
        Script {
            events: vec![
                (Duration::from_millis(2), tool_start(0, "core-child-list")),
                (Duration::from_millis(2), stop_tool_use()),
                (Duration::from_millis(2), message_stop()),
            ],
            close_after: true,
            status: 200,
            body: String::new(),
        },
        Script {
            events: vec![
                (Duration::from_millis(2), text_delta("子はいません")),
                (Duration::from_millis(2), end_turn()),
                (Duration::from_millis(2), message_stop()),
            ],
            close_after: true,
            status: 200,
            body: String::new(),
        },
    ];
    let (base, cert) = spawn_fake_tls(scripts);
    let engine: Arc<dyn Engine> = Arc::new(HttpSseEngine::with_client(
        base,
        Box::new(AnthropicProvider::new("m", "", 64)),
        client_trusting(&cert),
    ));
    let store = Store::new_in_memory().unwrap();
    // このテストの provider はダミーモデル "m"。予算の物差し（§06）に context_window を登録する
    // （boot_with の seed は既知モデルだけなので、テスト固有の "m" はここで足す）。
    store.register_model_context_window("m", 200_000).unwrap();
    let host = Host::boot_with(store, engine, short_idle_cfg());
    let (p, _a, human) = firing_place(&host);

    host.sys
        .deliver(p, Incoming::said(human, "子は？"))
        .unwrap();
    let sys = host.sys.clone();
    // said(1) + 最終の spoke(2) まで進むのを待つ。
    assert!(
        wait_until(Duration::from_secs(6), || sys
            .store()
            .latest_seq(p)
            .unwrap()
            >= 2)
        .await,
        "道具を呼んだあと、同じターンで再推論して発話が確定する"
    );
    let recs = host.sys.store().turn_records(p).unwrap();
    assert_eq!(
        recs[0].iterations, 2,
        "道具呼び出しでターンが継続し、2 反復回る（§07: 結果の還流→再推論）"
    );
    assert_eq!(recs[0].end_reason, "done");
    let spoke = host.sys.store().get_event(p, 2).unwrap().unwrap();
    assert_eq!(
        spoke.content.text.as_deref(),
        Some("子はいません"),
        "道具の結果を読んだ最終の発話が確定する"
    );
}

// 5) 広告の wire 形: ctx.tools が build_body の `tools` に写る（input_schema 付き・末尾に cache_control）。
//    宣言しない道具をモデルは呼べないので、この写しが「本物のエージェントが道具を使える」下地（§10）。
#[test]
fn build_body_declares_tools_with_schema_and_cache() {
    let provider = AnthropicProvider::new("m", "k", 64);
    let ctx = Context {
        rendered: "hi".to_string(),
        tools: vec![
            ToolDef {
                name: "core-child-list".to_string(),
                description: "子の一覧".to_string(),
                params: json!({"type":"object","properties":{}}),
            },
            ToolDef {
                name: "core-read-log".to_string(),
                description: "ログを読む".to_string(),
                params: json!({"type":"object","properties":{"from":{"type":"integer"}}}),
            },
        ],
        ..Context::default()
    };
    let body = provider.build_body(&ctx);
    let tools = body["tools"].as_array().expect("tools 配列がある");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "core-child-list");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    // ツール列の末尾にだけ cache_control（安定プレフィックスのキャッシュ）。
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

    // 道具が無ければ tools 欄を出さない（空配列すら出さない）。
    let empty = provider.build_body(&Context {
        rendered: "hi".to_string(),
        ..Context::default()
    });
    assert!(empty.get("tools").is_none(), "道具が無ければ tools は無い");
}

/// ctx.tools を記録するだけの engine（広告が engine の口まで届くことを見る）。
#[derive(Clone)]
struct CapturingEngine {
    seen: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl Engine for CapturingEngine {
    fn model(&self) -> &str {
        // boot_with が seed する既知モデル。予算の物差しは echo と同じで足りる（この test は道具広告を見る）。
        opencrab_app::ECHO_MODEL
    }

    async fn infer(&self, ctx: &Context, chunks: &ChunkSink) -> Result<InferOutput, EngineError> {
        chunks.chunk();
        *self.seen.lock().unwrap() = ctx.tools.iter().map(|t| t.name.clone()).collect();
        Ok(InferOutput {
            effects: vec![EffectSpec::say("ok")],
            tool_calls: vec![],
            done: true,
        })
    }
}

// 6) core が ctx.tools を組んで engine に渡す（§09/§10）。Trusted の Participant には core 道具が見える。
//    未実装の core-expand-tools は広告しない（呼べば失敗する道具を見せない・§15）。
#[tokio::test(flavor = "current_thread")]
async fn core_advertises_authorized_tools_to_engine() {
    let seen = Arc::new(Mutex::new(vec![]));
    let engine: Arc<dyn Engine> = Arc::new(CapturingEngine { seen: seen.clone() });
    let store = Store::new_in_memory().unwrap();
    let host = Host::boot_with(store, engine, Config::default());
    let (p, _a, human) = firing_place(&host);

    host.sys.deliver(p, Incoming::said(human, "go")).unwrap();
    let sys = host.sys.clone();
    assert!(
        wait_until(Duration::from_secs(5), || sys
            .store()
            .latest_seq(p)
            .unwrap()
            >= 2)
        .await,
        "ターンが動いて広告が engine に届く"
    );
    let names = seen.lock().unwrap().clone();
    assert!(
        names.contains(&"core-create-place".to_string())
            && names.contains(&"core-child-list".to_string())
            && names.contains(&"core-read-log".to_string()),
        "Trusted の Participant には core 道具が見える: {names:?}"
    );
    assert!(
        !names.contains(&"core-expand-tools".to_string()),
        "未実装の core-expand-tools は広告しない: {names:?}"
    );
}

// 6) system の写し先: core が組んだ system を、各プロバイダが自分の wire のスロットへ載せるだけ（設計）。
//    Anthropic=top-level `system`（block 配列・末尾に cache_control）／OpenAI chat=先頭 role=system／
//    Responses=`instructions`。空（非 Agent ターン等）のときはどれもスロットを出さない（線に空を載せない）。
#[test]
fn build_body_places_system_in_each_providers_slot() {
    const SYS: &str = "あなたはテストエージェントです。\n\nこの場のチャットに参加しています。";

    // --- Anthropic: top-level system（block 配列・cache_control 付き）---
    let anth = AnthropicProvider::new("m", "k", 64);
    let body = anth.build_body(&Context {
        system: SYS.to_string(),
        rendered: "hi".to_string(),
        ..Context::default()
    });
    let sys = body["system"].as_array().expect("system は block 配列");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["type"], "text");
    assert_eq!(sys[0]["text"], SYS);
    assert_eq!(
        sys[0]["cache_control"]["type"], "ephemeral",
        "system 末尾に cache breakpoint"
    );
    // 空 system のときは system スロットを出さない。
    let empty = anth.build_body(&Context {
        rendered: "hi".to_string(),
        ..Context::default()
    });
    assert!(empty.get("system").is_none(), "空 system は載せない");

    // --- OpenAI chat: 先頭 role=system、その後に user=rendered ---
    let oai = OpenAiProvider::new("m", "k", 64);
    let body = oai.build_body(&Context {
        system: SYS.to_string(),
        rendered: "hi".to_string(),
        ..Context::default()
    });
    let msgs = body["messages"].as_array().expect("messages 配列");
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], SYS);
    assert_eq!(msgs[1]["role"], "user", "system の後に rendered の user");
    assert_eq!(msgs[1]["content"], "hi");
    // 空 system のときは先頭が user（system メッセージを入れない）。
    let empty = oai.build_body(&Context {
        rendered: "hi".to_string(),
        ..Context::default()
    });
    let msgs = empty["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user", "空 system なら先頭は user");

    // --- Responses（ChatGpt）: instructions ---
    let gpt = ChatGptProvider::new("m", "/nonexistent-auth.json");
    let body = gpt.build_body(&Context {
        system: SYS.to_string(),
        rendered: "hi".to_string(),
        ..Context::default()
    });
    assert_eq!(body["instructions"], SYS);
    // 空 system のときは instructions を出さない。
    let empty = gpt.build_body(&Context {
        rendered: "hi".to_string(),
        ..Context::default()
    });
    assert!(
        empty.get("instructions").is_none(),
        "空 system は instructions を出さない"
    );
}
