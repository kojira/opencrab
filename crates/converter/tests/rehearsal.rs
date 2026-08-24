//! C4/C5 rehearsal against a migrated copy (TEST-DESIGN).
//!
//! Gated on OPENCRAB_REHEARSAL_DB. If unset, return immediately — local-ci
//! does not carry a production copy. The path is never written into the repo.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "secret-token";
const TIMEOUT: Duration = Duration::from_secs(20);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rehearsal_db() -> Option<PathBuf> {
    match std::env::var("OPENCRAB_REHEARSAL_DB") {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

fn debug_bin(name: &str) -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("debug").join(name)
}

fn require_bin(name: &str) -> PathBuf {
    let path = debug_bin(name);
    assert!(
        path.is_file(),
        "required binary missing at {} (build the workspace first)",
        path.display()
    );
    path
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = auth {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
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
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

fn spawn_core(socket: &Path, db: &Path) -> Proc {
    let child = Command::new(require_bin("opencrab-social-runtime"))
        .arg(socket)
        .arg(db)
        .arg("room:main")
        .env("OPENCRAB_LLM_PROVIDER", "mock")
        .env("OPENCRAB_MOCK_LLM_SCRIPT", "reply")
        .env_remove("OPENCRAB_PLACES")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    Proc(child)
}

fn spawn_web(socket: &Path, port: u16) -> Proc {
    let child = Command::new(require_bin("web-gate-e2e"))
        .arg(socket)
        .arg(port.to_string())
        .arg(TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn web-gate");
    Proc(child)
}

fn spawn_admin(db: &Path, port: u16, web_dist: &Path) -> Proc {
    let child = Command::new(require_bin("admin-server"))
        .arg(db)
        .arg(port.to_string())
        .arg(web_dist)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn admin-server");
    Proc(child)
}

/// C4: a migrated copy starts runtime and completes a web conversation.
#[test]
fn migrated_copy_starts_runtime_and_web_conversation() {
    let Some(db) = rehearsal_db() else {
        return;
    };
    assert!(
        db.is_file(),
        "OPENCRAB_REHEARSAL_DB is not a file: {}",
        db.display()
    );
    let scratch = tempfile::tempdir().expect("scratch");
    let socket = scratch.path().join("r.sock");
    let port = free_port();
    let _core = spawn_core(&socket, &db);
    let _web = spawn_web(&socket, port);
    let start = Instant::now();
    let mut last_post = Instant::now() - Duration::from_secs(1);
    let mut history = String::new();
    while start.elapsed() < TIMEOUT {
        if let Some((_, body)) = http(port, "GET", "/rooms/main/messages?since=0", None, None) {
            history = body;
            if history.contains("mock reply") {
                return;
            }
        }
        if last_post.elapsed() >= Duration::from_millis(200) {
            let body = "{\"author\":\"test-owner\",\"text\":\"synthetic rehearsal question\"}";
            let _ = http(
                port,
                "POST",
                "/rooms/main/messages",
                Some(TOKEN),
                Some(body),
            );
            last_post = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("rehearsal copy must complete a web conversation: {history}");
}

/// C5: the same copy serves agents / sessions / llm_logs at 200.
#[test]
fn migrated_copy_serves_agents_sessions_llm_logs() {
    let Some(db) = rehearsal_db() else {
        return;
    };
    assert!(
        db.is_file(),
        "OPENCRAB_REHEARSAL_DB is not a file: {}",
        db.display()
    );
    let scratch = tempfile::tempdir().expect("scratch");
    let dist = scratch.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>synthetic</title>",
    )
    .unwrap();
    let port = free_port();
    let _admin = spawn_admin(&db, port, &dist);
    let start = Instant::now();
    let mut agents = None;
    while start.elapsed() < TIMEOUT {
        if let Some((status, body)) = http(port, "GET", "/api/agents", None, None) {
            if status == 200 {
                agents = Some(body);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let agents = agents.expect("GET /api/agents must become 200");
    let sessions = http(port, "GET", "/api/sessions", None, None).expect("GET /api/sessions");
    assert_eq!(sessions.0, 200, "GET /api/sessions: {}", sessions.1);
    let id = first_agent_id(&agents).expect("rehearsal copy must list at least one agent");
    let logs = http(
        port,
        "GET",
        &format!("/api/agents/{id}/llm-logs"),
        None,
        None,
    )
    .expect("GET llm-logs");
    assert_eq!(logs.0, 200, "GET /api/agents/{id}/llm-logs: {}", logs.1);
}

fn first_agent_id(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}
