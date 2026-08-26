//! DESIGN-WEBGATE §7.4 connected / disconnected プロセス E2E。
//! Binding PUT は使わず POST /api/agents/{id}/web-conversations だけが binding を作る。

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

#[test]
fn connected_create_is_201_then_sse_said_turn_say() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-create-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);
    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");

    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some(r#"{"name":"E2E"}"#),
        Duration::from_secs(70),
    )
    .expect("create");
    assert_eq!(st, 201, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "ready");
    assert_eq!(v["name"], "E2E");
    let session = v["session_id"].as_str().unwrap().to_string();
    let binding = v["binding_id"].as_str().unwrap().to_string();
    assert_eq!(counts(&db), (1, 1, 1));

    let (st, detail) = http(
        core_port,
        "GET",
        &format!("/api/sessions/{session}"),
        None,
        None,
        Duration::from_secs(5),
    )
    .expect("detail");
    assert_eq!(st, 200, "{detail}");
    let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
    assert_eq!(d["web_binding_state"], "ready");

    let sse_rx = spawn_sse(gw_port, &session);
    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let (st, accepted) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("message");
    assert_eq!(st, 202, "{accepted}");
    let a: serde_json::Value = serde_json::from_str(accepted.trim()).unwrap();
    assert_eq!(a["state"], "accepted");
    mock.release();
    let sse = sse_rx.recv_timeout(Duration::from_secs(30)).expect("sse");
    assert!(
        sse.contains("event: message") && sse.contains(REPLY),
        "{sse}"
    );

    let conn = Connection::open(&db).unwrap();
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = 'hello from e2e'",
            [format!("extgate-{binding}")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, 1);
    let reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = ?2",
            [format!("extgate-{binding}"), REPLY.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply, 1);
    drop(gw);
    drop(core);
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn disconnected_create_is_202_then_ready_after_hello() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-disc-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);

    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some("{}"),
        Duration::from_secs(10),
    )
    .expect("create");
    assert_eq!(st, 202, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "provisioning");
    let session = v["session_id"].as_str().unwrap().to_string();
    assert_eq!(counts(&db), (1, 1, 1));

    let (st, detail) = http(
        core_port,
        "GET",
        &format!("/api/sessions/{session}"),
        None,
        None,
        Duration::from_secs(5),
    )
    .expect("detail");
    let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
    assert_eq!(st, 200);
    assert_eq!(d["web_binding_state"], "unavailable");

    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");

    let start = Instant::now();
    let mut ready = None;
    while start.elapsed() < Duration::from_secs(15) {
        if let Some((st, detail)) = http(
            core_port,
            "GET",
            &format!("/api/sessions/{session}"),
            None,
            None,
            Duration::from_secs(2),
        ) {
            if st == 200 {
                let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
                if d["web_binding_state"] == "ready" {
                    ready = Some(d);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready.is_some(), "detail never became ready");
    assert_eq!(
        counts(&db),
        (1, 1, 1),
        "must not duplicate the conversation"
    );

    mock.release();
    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let (st, accepted) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("message");
    assert_eq!(st, 202, "{accepted}");
    let a: serde_json::Value = serde_json::from_str(accepted.trim()).unwrap();
    assert_ne!(a["error"]["code"], "instance_not_ready", "{accepted}");
    assert_eq!(a["state"], "accepted", "{accepted}");
    drop(gw);
    drop(core);
    let _ = std::fs::remove_file(&sock);
}

fn wait_gateway_uds(gw_port: u16, timeout: Duration) -> Option<(u16, String)> {
    let probe = r#"{"client_message_id":"dddddddd-dddd-4ddd-8ddd-dddddddddddd","text":"probe","attachments":[]}"#;
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        if let Some((st, body)) = http(
            gw_port,
            "POST",
            "/api/web-conversations/web-e2eagent-probe/messages",
            None,
            Some(probe),
            Duration::from_secs(2),
        ) {
            last = Some((st, body.clone()));
            if st == 503 && body.contains("instance_not_ready") {
                return last;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    last
}

/// QC 症状の再現: core 再起動後も gateway が自動再接続し、作成が 201 ready → message → say。
#[test]
fn core_restart_gateway_reconnects_then_create_201_message_say() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-reconn-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);
    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");
    let live = wait_gateway_uds(gw_port, Duration::from_secs(15));
    assert!(
        live.as_ref()
            .is_some_and(|(st, b)| *st == 503 && b.contains("instance_not_ready")),
        "gateway never hellod before restart: {live:?}"
    );

    drop(core);
    let down_start = Instant::now();
    while down_start.elapsed() < Duration::from_secs(5) {
        if http(
            core_port,
            "GET",
            "/health",
            None,
            None,
            Duration::from_secs(1),
        )
        .is_none()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let cut_start = Instant::now();
    let mut cut = None;
    while cut_start.elapsed() < Duration::from_secs(8) {
        match http(
            gw_port,
            "POST",
            "/api/web-conversations/web-e2eagent-probe/messages",
            None,
            Some(
                r#"{"client_message_id":"dddddddd-dddd-4ddd-8ddd-dddddddddddd","text":"probe","attachments":[]}"#,
            ),
            Duration::from_secs(2),
        ) {
            Some((st, body)) => {
                assert_eq!(st, 503, "cut {body}");
                if body.contains("disconnect") {
                    cut = Some(body);
                    break;
                }
            }
            None => panic!("gateway HTTP died with core"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(cut.is_some(), "expected disconnect while core is down");

    let core = spawn_core(root.path());
    assert!(
        wait_http(core_port, "/health", Duration::from_secs(30)),
        "core did not restart"
    );
    let live = wait_gateway_uds(gw_port, Duration::from_secs(15));
    assert!(
        live.as_ref()
            .is_some_and(|(st, b)| *st == 503 && b.contains("instance_not_ready")),
        "gateway did not re-hello after core restart: {live:?}"
    );

    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some(r#"{"name":"Reconnect"}"#),
        Duration::from_secs(70),
    )
    .expect("create");
    assert_eq!(st, 201, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "ready", "{body}");
    let session = v["session_id"].as_str().unwrap().to_string();
    let binding = v["binding_id"].as_str().unwrap().to_string();

    let sse_rx = spawn_sse(gw_port, &session);
    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let (st, accepted) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("message");
    assert_eq!(st, 202, "{accepted}");
    let a: serde_json::Value = serde_json::from_str(accepted.trim()).unwrap();
    assert_eq!(a["state"], "accepted", "{accepted}");
    mock.release();
    let sse = sse_rx.recv_timeout(Duration::from_secs(30)).expect("sse");
    assert!(
        sse.contains("event: message") && sse.contains(REPLY),
        "{sse}"
    );

    drop(gw);
    drop(core);
    let conn = Connection::open(&db).unwrap();
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = 'hello from e2e'",
            [format!("extgate-{binding}")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, 1);
    let reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = ?2",
            [format!("extgate-{binding}"), REPLY.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply, 1);
    let _ = std::fs::remove_file(&sock);
}
