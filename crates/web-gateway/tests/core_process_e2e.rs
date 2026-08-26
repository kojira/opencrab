//! DESIGN-WEBGATE §5.1: 実 opencrab-server + 実 UDS + web-gateway プロセス。
//! send → said → turn → say → SSE 確定を 1 本で見る。LLM は injection（解放ゲート付き mock）。

use rusqlite::Connection;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const AGENT: &str = "e2eagent";
const AUTHOR: &str = "e2e-owner";
const LOGICAL: &str = "web-e2eagent-c1";
const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";
const BINDING: &str = "22222222-2222-4222-8222-222222222222";
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
    assert!(
        server.exists(),
        "opencrab-server missing at {}",
        server.display()
    );
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

#[test]
fn send_said_turn_say_sse_over_real_processes() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-e2e-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    assert!(sock.as_os_str().len() < 100, "socket path too long");
    let core_port = free_port();
    let gw_port = free_port();
    write_server_config(root.path(), &db, &sock, core_port, mock.port);

    let core_log = root.path().join("core.log");
    let core_out = std::fs::File::create(&core_log).unwrap();
    let core_err = core_out.try_clone().unwrap();
    let core = Proc(
        Command::new(server_bin())
            .current_dir(root.path())
            .env("OPENCRAB_GATE_OPERATOR_TOKEN", TOKEN)
            .env(
                "RUST_LOG",
                "opencrab=debug,opencrab_server=debug,opencrab_extgate=debug,opencrab_core=debug",
            )
            .env_remove("OPENCRAB_SECRET_MASTER_KEY")
            .stdout(Stdio::from(core_out))
            .stderr(Stdio::from(core_err))
            .spawn()
            .expect("spawn opencrab-server"),
    );
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
        Some(r#"{"provider":"openai","model":"e2e-mock","context_window":8192,"max_output_tokens":1024}"#),
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
    assert!(subject > 0, "{agent_json}");

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

    let bind_body = format!(r#"{{"instance_id":"{INSTANCE}","address":"{LOGICAL}"}}"#);
    let (st, body) = http(
        core_port,
        "PUT",
        &format!("/api/gate-bindings/{BINDING}"),
        Some(TOKEN),
        Some(&bind_body),
        Duration::from_secs(5),
    )
    .expect("binding put");
    assert!(st == 200 || st == 201, "binding PUT {st} {body}");

    let placement = root.path().join("placement.json");
    std::fs::write(
        &placement,
        format!(
            r#"{{"http_bind":"127.0.0.1:{gw_port}","core_socket":"{}","instances":[{{"instance_id":"{INSTANCE}","revision":1,"author_id":"{AUTHOR}"}}]}}"#,
            sock.display()
        ),
    )
    .unwrap();

    let gw = Proc(
        Command::new(env!("CARGO_BIN_EXE_web-gateway"))
            .arg(&placement)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn web-gateway"),
    );

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if TcpStream::connect(("127.0.0.1", gw_port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        TcpStream::connect(("127.0.0.1", gw_port)).is_ok(),
        "web-gateway HTTP did not start"
    );
    let sse_rx = spawn_sse(gw_port, LOGICAL);

    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let start = Instant::now();
    let mut accepted = None;
    while start.elapsed() < Duration::from_secs(15) {
        if let Some((st, body)) = http(
            gw_port,
            "POST",
            &format!("/api/web-conversations/{LOGICAL}/messages"),
            None,
            Some(&post_body),
            Duration::from_secs(5),
        ) {
            if st == 202 {
                accepted = Some(body);
                break;
            }
            assert!(
                st == 503,
                "unexpected POST status before bind/said: {st} {body}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let accepted = accepted.expect("POST never reached 202");
    let v: serde_json::Value = serde_json::from_str(accepted.trim()).expect("202 json");
    assert_eq!(v["state"], "accepted", "{accepted}");
    assert_eq!(v["origin"], format!("web:{CLIENT_MSG}"));
    assert!(v["seq"].as_i64().unwrap_or(0) > 0, "{accepted}");

    match sse_rx.try_recv() {
        Ok(early) => panic!("SSE arrived before LLM release: {early}"),
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(e) => panic!("SSE thread died before release: {e}"),
    }

    mock.release();
    let sse = match sse_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(s) => s,
        Err(e) => {
            let log = std::fs::read_to_string(&core_log).unwrap_or_default();
            let tail: String = log
                .chars()
                .rev()
                .take(4000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            panic!("SSE after LLM release: {e}\n--- core stderr tail ---\n{tail}");
        }
    };
    if !sse.contains("event: message") || !sse.contains(REPLY) {
        drop(gw);
        drop(core);
        let log = std::fs::read_to_string(&core_log).unwrap_or_default();
        let tail: String = log
            .chars()
            .rev()
            .take(6000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let conn = Connection::open(&db).ok();
        let extra = if let Some(conn) = conn {
            let logs: Vec<String> = conn
                .prepare("SELECT log_type || ':' || substr(content,1,80) FROM memory_sessions")
                .ok()
                .and_then(|mut s| {
                    s.query_map([], |r| r.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();
            let llm: Vec<String> = conn
                .prepare("SELECT substr(ifnull(error_body,''),1,200) || ' / ' || substr(ifnull(response,''),1,80) FROM llm_logs")
                .ok()
                .and_then(|mut s| {
                    s.query_map([], |r| r.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();
            format!("memory_sessions={logs:?}\nllm_logs={llm:?}")
        } else {
            "db closed".into()
        };
        panic!("SSE did not confirm say: {sse}\n{extra}\n--- core stderr ---\n{tail}");
    }

    drop(gw);
    drop(core);
    let conn = Connection::open(&db).expect("open db after stop");
    let origins: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM external_origins WHERE binding_id = ?1 AND origin = ?2",
            [BINDING, &format!("web:{CLIENT_MSG}")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(origins, 1, "external_origins");
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = 'hello from e2e'",
            [format!("extgate-{BINDING}")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, 1, "inbound log");
    let reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = ?2",
            [format!("extgate-{BINDING}"), REPLY.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply, 1, "reply log");
    let delivered: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE state = 'delivered'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(delivered, 1, "deliveries");
    let sources: Vec<String> = conn
        .prepare("SELECT metadata_json FROM memory_sessions WHERE session_id = ?1")
        .unwrap()
        .query_map([format!("extgate-{BINDING}")], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap()
        .map(|r| r.unwrap().unwrap_or_default())
        .collect();
    assert!(
        sources
            .iter()
            .any(|m| m.contains("\"source\":\"external\"")),
        "inbound source: {sources:?}"
    );
    assert!(
        sources
            .iter()
            .any(|m| m.contains("\"source\":\"external_response\"")),
        "reply source: {sources:?}"
    );
    let _ = std::fs::remove_file(&sock);
}
