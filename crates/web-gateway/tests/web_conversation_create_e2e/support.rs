
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
const CLIENT_MSG: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const REPLY: &str = "e2e-reply-from-mock";
const CONFIG_B64: &str = "eyJhdXRob3JfaWQiOiJlMmUtb3duZXIifQ==";

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct MockLlm {
    port: u16,
    release: Arc<(Mutex<bool>, Condvar)>,
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

fn spawn_mock_llm() -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock llm bind");
    let port = listener.local_addr().unwrap().port();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let rel = release.clone();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else {
                continue;
            };
            let head = read_http_request(&mut stream);
            if head.contains("chat/completions") {
                let (lock, cv) = &*rel;
                let mut ready = lock.lock().unwrap();
                while !*ready {
                    ready = cv.wait(ready).unwrap();
                }
                let body = format!(
                    r#"{{"id":"e2e","choices":[{{"index":0,"message":{{"role":"assistant","content":"{REPLY}"}},"finish_reason":"stop"}}]}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            } else {
                let body = r#"{"data":[]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        }
    });
    MockLlm { port, release }
}

impl MockLlm {
    fn release(&self) {
        let (lock, cv) = &*self.release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

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

fn write_server_config(root: &Path, db: &Path, sock: &Path, http_port: u16, llm_port: u16) {
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
auto_dispatch = false

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

[tools]
enabled = false

[llm_log_archive]
enabled = false
"#,
            llm_port = llm_port,
            http_port = http_port,
            db = db.display(),
            sock = sock.display(),
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

fn spawn_sse(port: u16, session: &str) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let session = session.to_string();
    std::thread::spawn(move || {
        let connect_until = Instant::now() + Duration::from_secs(20);
        let mut buf = Vec::new();
        let mut stream = loop {
            if Instant::now() > connect_until {
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
        let until = Instant::now() + Duration::from_secs(40);
        while Instant::now() < until {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if text.contains("event: message") || text.contains("event: gate_error") {
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
    });
    rx
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
                "opencrab=info,opencrab_server=info,opencrab_extgate=info",
            )
            .env_remove("OPENCRAB_SECRET_MASTER_KEY")
            .stdout(Stdio::from(core_out))
            .stderr(Stdio::from(core_err))
            .spawn()
            .expect("spawn opencrab-server"),
    )
}

fn seed_core(root: &Path, db: &Path, sock: &Path, core_port: u16, llm_port: u16) -> Proc {
    write_server_config(root, db, sock, core_port, llm_port);
    // #826: core は起動時 `ensure_startup_budget_inputs`（fail-loud）で既定モデルの
    // `model_pricing` を要求する。DB 実体は core が作るが、その前にここで schema を作り
    // e2e-mock を seed しておかないと core が起動時に落ちて HTTP が上がらない（30s タイムアウト）。
    // context_window は予算 envelope の mandatory_fixed（ツール ~7.1k 等）を input_high に収める
    // ため 200000（8192 では context_budget_exhausted）。seed 接続の WAL を畳んでから閉じ、直後の
    // core 側 r2d2 プールが "database is locked" をリトライして起動が遅れるのを防ぐ。
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
    let core = spawn_core(root);
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
    {
        let conn = Connection::open(db).expect("open db for owner");
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
    core
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
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn web-gateway"),
    )
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

fn counts(db: &Path) -> (i64, i64, i64) {
    let conn = Connection::open(db).unwrap();
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id LIKE 'extgate-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let members: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_sessions", [], |r| r.get(0))
        .unwrap();
    let bindings: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    (sessions, members, bindings)
}
