//! issue #788 残 3 契約の web mock E2E（実 LLM 不要・決定的）。
//!
//! e2e_local.rs（実 LLM opt-in）のシナリオを、実プロセス（opencrab-server + web-gateway）+
//! HTTP mock LLM（OpenAI 互換）へ移植する。qc_harness_e2e.rs の `RoutedMock`（内容ルーティング /
//! Notify で長処理を保持）を、プロセス越えの HTTP mock として作り直したもの。
//!
//! 固定する 3 契約:
//!   1. NO_REPLY/withheld: `content:"NO_REPLY"` の応答は SSE に `event: message` として
//!      流れず、`event: completed_no_reply` として観測でき、DB に `no_reply` として残る。
//!   2. 非ブロッキング: 長処理（保持中の背景サブタスク）走行中に第2依頼が待たされず即応する
//!      （qc_harness_e2e の scenario_main の web 版）。
//!   3. 未許可コマンド拒否: allowlist に無いコマンドの execute_shell が拒否され、拒否理由が
//!      エラー契約どおり（"is not in the allowed list"）会話へ残る。
//!
//! mock LLM が返す JSON 形は `crates/llm/src/providers/openai_compat.rs` のパーサに従う
//! （tool_calls[].function.arguments は JSON 文字列・finish_reason は "tool_calls"/"stop"）。

use rusqlite::Connection;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const AGENT: &str = "e2eagent";
const AUTHOR: &str = "e2e-owner";
const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";
const TOKEN: &str = "e2e-operator-token";
// base64 of `{"author_id":"e2e-owner"}`。web author=owner に解決させ caller=Owner にする。
const CONFIG_B64: &str = "eyJhdXRob3JfaWQiOiJlMmUtb3duZXIifQ==";

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ==================== HTTP mock LLM（内容ルーティング・接続ごとにスレッド） ====================

/// 解放ゲート。長処理サブタスク sub-run を release まで保持するのに使う。
type Gate = Arc<(Mutex<bool>, Condvar)>;

struct MockLlm {
    port: u16,
    gate: Gate,
}

impl MockLlm {
    /// 保持中の sub-run を解放する。
    fn release(&self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

/// gate が release されるまでブロックする（sub-run の「長処理」を表す）。
fn wait_release(gate: &Gate) {
    let (lock, cv) = &**gate;
    let mut ready = lock.lock().unwrap();
    while !*ready {
        ready = cv.wait(ready).unwrap();
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = &buf[..header_end];
                    let content_len = String::from_utf8_lossy(headers)
                        .lines()
                        .find_map(|l| {
                            l.split_once(':').and_then(|(k, v)| {
                                (k.eq_ignore_ascii_case("content-length"))
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    if buf.len() >= header_end + 4 + content_len {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// chat/completions のリクエスト全文（headers+body）と gate を受け取り、返す OpenAI 応答 JSON を
/// 決める router を張って mock を起動する。models 系は空一覧を返す。
fn spawn_mock<F>(router: F) -> MockLlm
where
    F: Fn(&str, &Gate) -> String + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock llm bind");
    let port = listener.local_addr().unwrap().port();
    let gate: Gate = Arc::new((Mutex::new(false), Condvar::new()));
    let router = Arc::new(router);
    let gate_srv = gate.clone();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else {
                continue;
            };
            // 保持中の sub-run が accept ループを塞がないよう、接続ごとに 1 スレッド。
            let router = router.clone();
            let gate = gate_srv.clone();
            std::thread::spawn(move || {
                let head = read_http_request(&mut stream);
                let body = if head.contains("chat/completions") {
                    router(&head, &gate)
                } else {
                    r#"{"data":[]}"#.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    MockLlm { port, gate }
}

/// テキスト（stop）応答の OpenAI JSON。
fn text_resp(content: &str) -> String {
    serde_json::json!({
        "id": "e2e",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// tool_call（tool_calls / finish_reason=tool_calls）応答の OpenAI JSON。
/// arguments は JSON 文字列で載せる（openai_compat パーサ要件）。
fn tool_call_resp(name: &str, args: serde_json::Value) -> String {
    serde_json::json!({
        "id": "e2e",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string()
}

/// リクエストにツール結果メッセージ（role:"tool"）が含まれるか。ツール実行後の継続ターン判定。
fn has_tool_result(req: &str) -> bool {
    req.contains("\"tool_call_id\"") || req.contains("\"role\":\"tool\"")
}

// ==================== プロセス起動・設定 ====================

fn server_bin() -> PathBuf {
    let gw = PathBuf::from(env!("CARGO_BIN_EXE_web-gateway"));
    let server = gw.parent().unwrap().join("opencrab-server");
    if !server.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "opencrab-server", "--bin", "opencrab-server"])
            .status()
            .expect("cargo build opencrab-server");
        assert!(status.success(), "opencrab-server build failed");
    }
    server
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 設定を書く。`auto_dispatch`（[subtask]）と `tools_block`（[tools]/[tools.shell]）を差し込む。
fn write_server_config(
    root: &Path,
    db: &Path,
    sock: &Path,
    http_port: u16,
    llm_port: u16,
    auto_dispatch: bool,
    tools_block: &str,
) {
    let cfg_dir = root.join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("default.toml"),
        format!(
            r#"
[agent]
heartbeat_interval_secs = 1800
heartbeat_enabled = false
workspace_path = "data/agents/{{agent_id}}/workspace"
max_workspace_size_mb = 100

[subtask]
auto_dispatch = {auto_dispatch}

[llm]
default_provider = "openai"
default_model = "e2e-mock"

[llm.self_selection]
enabled = false
allowed_aliases = []

[llm.fallback]
chain = []

[llm.providers.openai]
api_key = "dummy"
base_url = "http://127.0.0.1:{llm_port}/v1"
organization = ""

[gateway.rest]
enabled = true
port = {http_port}

[gateway.discord]
enabled = false
token = ""
guild_ids = []
agent_ids = []
owner_discord_id = ""

[dashboard]
enabled = false
port = 3000

[database]
path = "{db}"

[gate]
listen_socket = "{sock}"

{tools_block}

[llm_log_archive]
enabled = false
"#,
            auto_dispatch = auto_dispatch,
            llm_port = llm_port,
            http_port = http_port,
            db = db.display(),
            sock = sock.display(),
            tools_block = tools_block,
        ),
    )
    .unwrap();
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
    timeout: Duration,
) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).ok()?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).ok()?;
    let text = String::from_utf8_lossy(&resp).to_string();
    let status: u16 = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

fn wait_http(port: u16, path: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if http(port, "GET", path, None, None, Duration::from_secs(1)).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_tcp(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn spawn_core(root: &Path) -> Proc {
    let core_log = root.join("core.log");
    let core_out = std::fs::File::create(&core_log).unwrap();
    let core_err = core_out.try_clone().unwrap();
    Proc(
        Command::new(server_bin())
            .current_dir(root)
            .env("OPENCRAB_GATE_OPERATOR_TOKEN", TOKEN)
            .env(
                "RUST_LOG",
                "opencrab=info,opencrab_server=info,opencrab_extgate=info,opencrab_core=info",
            )
            .env_remove("OPENCRAB_SECRET_MASTER_KEY")
            .stdout(Stdio::from(core_out))
            .stderr(Stdio::from(core_err))
            .spawn()
            .expect("spawn opencrab-server"),
    )
}

fn spawn_gateway(root: &Path, sock: &Path, gw_port: u16) -> Proc {
    let placement = root.join("placement.json");
    std::fs::write(
        &placement,
        format!(
            r#"{{"http_bind":"127.0.0.1:{gw_port}","core_socket":"{}","instances":[{{"instance_id":"{INSTANCE}","revision":1,"author_id":"{AUTHOR}"}}]}}"#,
            sock.display()
        ),
    )
    .unwrap();
    Proc(
        Command::new(env!("CARGO_BIN_EXE_web-gateway"))
            .arg(&placement)
            .env("RUST_LOG", "web_gateway=info,opencrab_web_gateway=info")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn web-gateway"),
    )
}

/// テスト共通の土台。core+gateway を実プロセスで立て、connected create（201 ready）で
/// 会話（session_id / binding_id）を作って返す。
struct Harness {
    _core: Proc,
    _gw: Proc,
    db: PathBuf,
    sock: PathBuf,
    gw_port: u16,
    session: String,
    binding: String,
    _dir: tempfile::TempDir,
}

fn setup(mock_port: u16, tag: &str, auto_dispatch: bool, tools_block: &str) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-mock-{tag}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    assert!(sock.as_os_str().len() < 100, "socket path too long");
    let core_port = free_port();
    let gw_port = free_port();
    write_server_config(
        root.path(),
        &db,
        &sock,
        core_port,
        mock_port,
        auto_dispatch,
        tools_block,
    );

    // #826: core 起動時の予算チェック（fail-loud）を満たすため、起動前に schema+model_pricing を
    // seed する。context_window は mandatory_fixed を input_high に収めるため 200000。WAL を畳んで
    // 閉じ、直後の core 側 r2d2 プールが "database is locked" をリトライして起動が遅れるのを防ぐ。
    {
        let conn =
            opencrab_db::init_connection(db.to_str().unwrap()).expect("init db schema for seed");
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "openai".into(),
                model: "e2e-mock".into(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(1024),
            },
        )
        .expect("seed model_pricing for startup budget check");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
            .expect("checkpoint seed db");
    }

    let core = spawn_core(root.path());
    assert!(
        wait_http(core_port, "/health", Duration::from_secs(30)),
        "core HTTP did not start"
    );

    let (st, body) = http(
        core_port,
        "POST",
        "/api/agents",
        None,
        Some(r#"{"id":"e2eagent","name":"e2e","persona_name":"e2e"}"#),
        Duration::from_secs(5),
    )
    .expect("create agent");
    assert_eq!(st, 200, "{body}");

    // owner_discord_id=AUTHOR にして web author(e2e-owner)=owner を成立させる（caller=Owner）。
    {
        let conn = Connection::open(&db).expect("open db for owner");
        conn.execute(
            "INSERT INTO agent_discord_config (agent_id, bot_token, owner_discord_id, enabled, updated_at)
             VALUES (?1, 'x', ?2, 0, datetime('now'))",
            [AGENT, AUTHOR],
        )
        .unwrap();
    }

    let (st, body) = http(
        core_port,
        "PUT",
        "/api/llm/model-pricing",
        None,
        Some(r#"{"provider":"openai","model":"e2e-mock","context_window":200000,"max_output_tokens":1024}"#),
        Duration::from_secs(5),
    )
    .expect("model pricing");
    assert_eq!(st, 200, "model-pricing {body}");

    let (_, agent_json) = http(
        core_port,
        "GET",
        &format!("/api/agents/{AGENT}"),
        None,
        None,
        Duration::from_secs(5),
    )
    .expect("get agent");
    let agent_v: serde_json::Value = serde_json::from_str(agent_json.trim()).expect("agent json");
    let subject = agent_v["subject_id"].as_i64().expect("subject_id");

    let inst_body = format!(
        r#"{{"kind_id":"web","subject_id":{subject},"enabled":true,"config_b64":"{CONFIG_B64}"}}"#
    );
    let (st, body) = http(
        core_port,
        "PUT",
        &format!("/api/gate-instances/{INSTANCE}"),
        Some(TOKEN),
        Some(&inst_body),
        Duration::from_secs(5),
    )
    .expect("instance put");
    assert!(st == 200 || st == 201, "instance PUT {st} {body}");

    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");

    // connected create: gateway 接続済みなら 201 ready で binding+session を返す。
    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some(r#"{"name":"E2E"}"#),
        Duration::from_secs(70),
    )
    .expect("create web-conversation");
    assert_eq!(st, 201, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "ready", "{body}");
    let session = v["session_id"].as_str().unwrap().to_string();
    let binding = v["binding_id"].as_str().unwrap().to_string();

    Harness {
        _core: core,
        _gw: gw,
        db,
        sock,
        gw_port,
        session,
        binding,
        _dir: root,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// 会話へ 1 通投稿する（202 accepted）。
fn post_message(gw_port: u16, session: &str, client_msg_id: &str, text: &str) {
    let post_body = serde_json::json!({
        "client_message_id": client_msg_id,
        "text": text,
        "attachments": [],
    })
    .to_string();
    let (st, body) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("post message");
    assert_eq!(st, 202, "post message: {body}");
    let a: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(a["state"], "accepted", "{body}");
}

// ==================== DB ヘルパ（読み取り専用アサート） ====================

fn open_db(db: &Path) -> Connection {
    let conn = Connection::open(db).expect("open db");
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

/// memory_sessions で content LIKE %needle% の最小 id（無ければ None）。順序判定に使う。
fn first_log_id(db: &Path, session: &str, needle: &str) -> Option<i64> {
    let conn = open_db(db);
    conn.query_row(
        "SELECT MIN(id) FROM memory_sessions WHERE session_id = ?1 AND content LIKE ?2",
        rusqlite::params![session, format!("%{needle}%")],
        |r| r.get::<_, Option<i64>>(0),
    )
    .unwrap_or(None)
}

/// llm_logs.prompt に needle を含む行数（ツール結果の再注入確認などに使う）。
fn llm_prompt_hits(db: &Path, needle: &str) -> i64 {
    let conn = open_db(db);
    conn.query_row(
        "SELECT COUNT(*) FROM llm_logs WHERE prompt LIKE ?1",
        rusqlite::params![format!("%{needle}%")],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// pred が真になるまで最大 timeout ポーリング。
fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    pred()
}

// ==================== SSE コレクタ ====================

/// SSE に接続し、終端イベント（message / completed_no_reply / gate_error）を観測したら
/// それまでのバッファを送る。観測できなければ ~15s 後にバッファをそのまま送る。
fn spawn_sse_collect(port: u16, session: &str) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let session = session.to_string();
    std::thread::spawn(move || {
        let connect_until = Instant::now() + Duration::from_secs(20);
        let mut buf = Vec::new();
        let mut stream = loop {
            if Instant::now() > connect_until {
                let _ = tx.send(String::new());
                return;
            }
            let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            let req = format!(
                "GET /api/web-conversations/{session}/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n"
            );
            if stream.write_all(req.as_bytes()).is_err() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut hdr = Vec::new();
            let mut tmp = [0u8; 512];
            let hdr_until = Instant::now() + Duration::from_secs(2);
            let mut status = 0u16;
            while Instant::now() < hdr_until {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        hdr.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&hdr);
                        if text.contains("\r\n\r\n") {
                            status = text
                                .split_whitespace()
                                .nth(1)
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            break;
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
            if status == 200 {
                let text = String::from_utf8_lossy(&hdr);
                if let Some(pos) = text.find("\r\n\r\n") {
                    buf.extend_from_slice(&hdr[pos + 4..]);
                }
                break stream;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut tmp = [0u8; 2048];
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if text.contains("event: message")
                        || text.contains("event: completed_no_reply")
                        || text.contains("event: gate_error")
                    {
                        let _ = tx.send(text.into_owned());
                        return;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

// ==================== 契約 1: NO_REPLY / withheld ====================

/// mock が `content:"NO_REPLY"` を返すターンは、SSE に `event: message` として流れず
/// `event: completed_no_reply` として観測でき、say / delivery は起きない。
///
/// #899 §12.6: 沈黙の監査行 `speech='NO_REPLY'`（旧: `no_reply:true`）も **DB に残さない**。
/// withheld の可視化は SSE `completed_no_reply`（`LiveEvent::CompletedNoReply` 由来・DB 非依存）が担う。
#[test]
fn no_reply_is_withheld_not_said() {
    // 何が来ても沈黙（NO_REPLY）を返す。
    let mock = spawn_mock(|_req, _gate| text_resp("NO_REPLY"));
    let h = setup(mock.port, "noreply", false, "[tools]\nenabled = false\n");
    let session = &h.session;
    let binding = &h.binding;
    let db = &h.db;
    let ext_session = format!("extgate-{binding}");

    // SSE を張ってから投稿し、このターンのイベントを拾う。
    let sse_rx = spawn_sse_collect(h.gw_port, session);
    std::thread::sleep(Duration::from_millis(300));
    post_message(
        h.gw_port,
        session,
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "沈黙して",
    );

    // 沈黙の決着シグナルは SSE completed_no_reply（DB 行に依存しない・http.rs の
    // LiveEvent::CompletedNoReply 由来）。これを決着バリアに使う。
    let sse = sse_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("sse buffer");
    assert!(
        sse.contains("event: completed_no_reply"),
        "SSE に completed_no_reply が来ない: {sse:?}"
    );
    assert!(
        !sse.contains("event: message"),
        "NO_REPLY なのに SSE へ say(message) が流れた: {sse:?}"
    );

    // #899 §12.6: NO_REPLY のみは speech を保存しない（旧実装は withheld 監査行
    // content='NO_REPLY' / no_reply:true を残していたが、typed 履歴へ assistant 'NO_REPLY' として
    // 再注入されるため撤去）。沈黙の可視化は上の SSE completed_no_reply が担う。
    let no_reply_rows: i64 = {
        let conn = open_db(db);
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions
             WHERE session_id = ?1 AND content = 'NO_REPLY' AND speaker_id = ?2",
            rusqlite::params![ext_session, AGENT],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    assert_eq!(
        no_reply_rows, 0,
        "NO_REPLY のみなのに speech='NO_REPLY' が保存された（#899・§12.6）: {no_reply_rows}"
    );

    // say（external_response）は記録されていない。
    let says: i64 = {
        let conn = open_db(db);
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions
             WHERE session_id = ?1 AND metadata_json LIKE '%external_response%'",
            rusqlite::params![ext_session],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    assert_eq!(says, 0, "NO_REPLY なのに say が記録された");

    // deliveries（say の配送）も無い。
    let delivered: i64 = {
        let conn = open_db(db);
        conn.query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
            .unwrap_or(0)
    };
    assert_eq!(delivered, 0, "NO_REPLY なのに delivery が作られた");
}

/// 裁定A の web 退行固定: 返信した（say を出す）ターンでは SSE に `event: message` が流れ、
/// `event: completed_no_reply` は**流れない**。旧実装は activity ended を say より先に出していたため
/// 返信ターンでも completed_no_reply を誤発火していた（core reorder で撤去）。
#[test]
fn reply_turn_does_not_emit_completed_no_reply() {
    const B_REPLY: &str = "replybody-omega-answer";
    // 何が来ても通常の返信本文（＝say）を返す。
    let mock = spawn_mock(|_req, _gate| text_resp(B_REPLY));
    let h = setup(mock.port, "replyturn", false, "[tools]\nenabled = false\n");
    let session = &h.session;

    let sse_rx = spawn_sse_collect(h.gw_port, session);
    std::thread::sleep(Duration::from_millis(300));
    post_message(
        h.gw_port,
        session,
        "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        "返事して",
    );

    let sse = sse_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("sse buffer");
    assert!(
        sse.contains("event: message"),
        "返信ターンなのに SSE へ message(say) が流れない: {sse:?}"
    );
    assert!(
        !sse.contains("event: completed_no_reply"),
        "返信ターンで completed_no_reply が誤発火（裁定A の core reorder が効いていない）: {sse:?}"
    );
}

// ==================== 契約 2: 非ブロッキング（長処理中の第2依頼） ====================

// 会話へ現れるルーティング用マーカー（互いに部分文字列にならない）。
const M_FIRST: &str = "MARKERONE-longop";
const M_SECOND: &str = "MARKERTWO-question";
const M_SUBTASK: &str = "MARKERSUB-worktask";
// say として観測する応答本文（マーカーとも互いとも部分一致しない ASCII）。
const B_ACK: &str = "ackbody-alpha-started";
const B_SECOND: &str = "secondbody-beta-answer";
const B_COMPLETION: &str = "completionbody-gamma-done";
const B_SUBTASK_RESULT: &str = "subresult-delta-internal";

/// qc_harness_e2e の scenario_main の web 版。長処理（保持中の背景サブタスク）走行中に投じた
/// 第2依頼が待たされず即応し、3 say が 1→2→3 の順で残る。
#[test]
fn second_request_not_blocked_during_long_op() {
    // ルーティングは qc の RoutedMock と同順（E→B→D→A→C）。
    let mock = spawn_mock(|req, gate| {
        // (E) subtask 決着後の resume → 完了報告 say(3)。B_SUBTASK_RESULT を最優先で見る。
        if req.contains(B_SUBTASK_RESULT) {
            return text_resp(B_COMPLETION);
        }
        // (B) 親ターン#1 の spawn_subtask 実行後（tool 結果あり）→ 即応 ack say(1)。
        if has_tool_result(req) {
            return text_resp(B_ACK);
        }
        // (D) 第2依頼 → 即応 say(2)。
        if req.contains(M_SECOND) {
            return text_resp(B_SECOND);
        }
        // (A) 親ターン#1 初回 → spawn_subtask で背景サブタスクを detach。
        if req.contains(M_FIRST) {
            return tool_call_resp(
                "spawn_subtask",
                serde_json::json!({
                    "task": format!("{M_SUBTASK} 長い処理を実行して"),
                    "timeout_secs": 120,
                }),
            );
        }
        // (C) 背景サブタスク sub-run → release まで保持（＝長処理の走行中）。
        if req.contains(M_SUBTASK) {
            wait_release(gate);
            return text_resp(B_SUBTASK_RESULT);
        }
        // その他（create の hello 等）は沈黙。
        text_resp("NO_REPLY")
    });
    let h = setup(
        mock.port,
        "nonblock",
        true, // auto_dispatch 有効（scenario_main と同じ）
        "[tools]\nenabled = false\n",
    );
    let db = &h.db;
    let ext_session = format!("extgate-{}", h.binding);

    // 1) 長処理依頼 → ack say(1) ＋ 背景サブタスク detach。
    post_message(
        h.gw_port,
        &h.session,
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        &format!("{M_FIRST} 長い処理して終わったら教えて"),
    );
    let ack_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_ACK).is_some()
    });
    assert!(ack_ready, "ack say(1) が出ない");
    // この時点でサブタスクは走行中（未 release）＝完了報告はまだ無い。
    assert!(
        first_log_id(db, &ext_session, B_COMPLETION).is_none(),
        "release 前に完了報告が出ている（hold が効いていない）"
    );

    // 2) 走行中に第2依頼 → ブロックされず即応 say(2)。
    post_message(
        h.gw_port,
        &h.session,
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        &format!("{M_SECOND} 2 足す 2 は?"),
    );
    let second_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_SECOND).is_some()
    });
    assert!(
        second_ready,
        "第2依頼が長処理にブロックされている（say(2) 未達）"
    );
    // 第2依頼が返った時点でもサブタスクはまだ走行中（未 release）＝完了報告は無い。
    assert!(
        first_log_id(db, &ext_session, B_COMPLETION).is_none(),
        "第2依頼処理中にサブタスクが既に完了している（非ブロック検証が無効）"
    );

    // 3) サブタスクを解放 → 決着 → resume → 完了報告 say(3)。
    mock.release();
    let completion_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_COMPLETION).is_some()
    });
    assert!(completion_ready, "完了報告 say(3) が出ない");

    // 3 say が 1→2→3 の順で並ぶ。
    let i1 = first_log_id(db, &ext_session, B_ACK).expect("ack id");
    let i2 = first_log_id(db, &ext_session, B_SECOND).expect("second id");
    let i3 = first_log_id(db, &ext_session, B_COMPLETION).expect("completion id");
    assert!(
        i1 < i2 && i2 < i3,
        "say の順序が 1→2→3 でない: ack={i1} second={i2} completion={i3}"
    );
}

// ==================== 契約 3: 未許可コマンド拒否 ====================

const M_SHELL: &str = "MARKERSHELL-run";

/// allowlist に無いコマンドの execute_shell が拒否され、拒否理由がエラー契約どおり
/// （"is not in the allowed list"）会話へ残る。
#[test]
fn unauthorized_shell_command_is_rejected() {
    // 初回は execute_shell(rm) を tool_call。拒否結果の再注入後は短い確認テキストで締める。
    let mock = spawn_mock(|req, _gate| {
        if has_tool_result(req) {
            return text_resp("rejected-ok 拒否を確認しました");
        }
        if req.contains(M_SHELL) {
            return tool_call_resp(
                "execute_shell",
                serde_json::json!({"command": "rm", "args": ["-rf", "/tmp/should-not-run-788"]}),
            );
        }
        text_resp("NO_REPLY")
    });
    // execute_shell は有効・allowlist は echo のみ（rm は未許可）。inline 実行のため auto_dispatch=false。
    let tools_block = "[tools]\nenabled = true\n\n[tools.shell]\nenabled = true\nallowed_commands = [\"echo\"]\ntimeout_secs = 30\n";
    let h = setup(mock.port, "shelldeny", false, tools_block);
    let db = &h.db;
    let ext_session = format!("extgate-{}", h.binding);

    post_message(
        h.gw_port,
        &h.session,
        "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        &format!("{M_SHELL} rm -rf を実行して"),
    );

    // 拒否理由がツール結果として再注入される（llm_logs.prompt に現れる）＝エラー契約の観測。
    let rejected = wait_until(Duration::from_secs(30), || {
        llm_prompt_hits(db, "is not in the allowed list") >= 1
    });
    assert!(
        rejected,
        "未許可コマンドの拒否（is not in the allowed list）が観測できない"
    );

    // 拒否理由には対象コマンド名 rm が入る（エラー契約の本文）。
    assert!(
        llm_prompt_hits(db, "Command 'rm' is not in the allowed list") >= 1,
        "拒否理由が契約どおりの本文（Command 'rm' is not in the allowed list）でない"
    );

    // 拒否後にターンが破綻せず締まる（継続ターンの確認 say が出る）。
    let closed = wait_until(Duration::from_secs(20), || {
        first_log_id(db, &ext_session, "rejected-ok").is_some()
    });
    assert!(closed, "拒否後の締めターンが出ない");
}
