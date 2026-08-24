//! Web process E2E against MockEngine (TEST-DESIGN A4 / A9 / B1 / G2).
//!
//! EchoEngine is not a B-layer pass. Every case sets OPENCRAB_LLM_PROVIDER=mock
//! and a known OPENCRAB_MOCK_LLM_SCRIPT.

use opencrab_port::{GateName, Role, Standing, SubjectKind};
use opencrab_store::Store;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN: &str = "secret-token";
const TIMEOUT: Duration = Duration::from_secs(15);

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .parent()
        .expect("core binary parent")
        .to_path_buf()
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    bin_dir().join(format!("we2e-{}-{}-{}", std::process::id(), n, name))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_core(socket: &Path, db: &Path, script: &str) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_opencrab-social-runtime"))
        .arg(socket)
        .arg(db)
        .arg("room:main")
        .env("OPENCRAB_LLM_PROVIDER", "mock")
        .env("OPENCRAB_MOCK_LLM_SCRIPT", script)
        .env_remove("OPENCRAB_PLACES")
        .env_remove("OPENCRAB_TEST_CLOCK_SOCKET")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn opencrab-social-runtime");
    let mut proc = Proc(child);
    if let Some(stderr) = proc.0.stderr.take() {
        std::thread::spawn(move || {
            for line in
                std::io::BufRead::lines(std::io::BufReader::new(stderr)).map_while(Result::ok)
            {
                eprintln!("[core] {line}");
            }
        });
    }
    proc
}

fn spawn_web(socket: &Path, port: u16) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_web-gate-e2e"))
        .arg(socket)
        .arg(port.to_string())
        .arg(TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn web-gate");
    Proc(child)
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

fn post(port: u16, author: &str, text: &str) -> Option<(u16, String)> {
    let body = format!("{{\"author\":\"{author}\",\"text\":\"{text}\"}}");
    http(
        port,
        "POST",
        "/rooms/main/messages",
        Some(TOKEN),
        Some(&body),
    )
}

fn get_history(port: u16) -> String {
    http(port, "GET", "/rooms/main/messages?since=0", None, None)
        .map(|(_, body)| body)
        .unwrap_or_default()
}

fn post_until_history_contains(
    port: u16,
    author: &str,
    text: &str,
    needle: &str,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    let mut last_post = Instant::now() - Duration::from_secs(1);
    while start.elapsed() < timeout {
        if get_history(port).contains(needle) {
            return true;
        }
        if last_post.elapsed() >= Duration::from_millis(200) {
            let _ = post(port, author, text);
            last_post = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn find_room_place(store: &Store, address: &str) -> Option<i64> {
    for place in store.all_open_places().ok()? {
        if let Ok(Some(row)) = store.get_place(place) {
            if row.address.as_deref() == Some(address) {
                return Some(place);
            }
        }
    }
    None
}

fn find_agent(store: &Store, place: i64) -> Option<i64> {
    for member in store.members(place).ok()? {
        if member.role != Role::Participant {
            continue;
        }
        if let Ok(Some(subject)) = store.get_subject(member.subject) {
            if subject.kind == SubjectKind::Agent {
                return Some(member.subject);
            }
        }
    }
    None
}

fn seed_owner_and_shell(db: &Path) {
    let store = Store::open(db).expect("open store to seed owner and shell grants");
    let place = find_room_place(&store, "room:main").expect("room:main after first boot");
    let agent = find_agent(&store, place).expect("web-agent after first boot");
    store
        .allow_tool(agent, "core-shell")
        .expect("allow core-shell");
    store.allow_command(agent, "date").expect("allow date");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i64;
    let owner = store
        .create_subject(
            SubjectKind::Human,
            "test-owner",
            "",
            "engine",
            Standing::Owner,
            now,
        )
        .expect("create owner");
    store
        .add_identity(owner, &GateName::new("web"), "test-owner")
        .expect("bind test-owner identity");
    store
        .join(place, owner, Role::Participant, 0, now)
        .expect("join owner");
    drop(store);
}

fn boot_mock(script: &str) -> (Proc, Proc, u16, PathBuf) {
    let socket = scratch("s.sock");
    let db = scratch("core.db");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&db);
    assert!(
        socket.as_os_str().len() < 100,
        "unix socket path too long: {}",
        socket.display()
    );
    let port = free_port();
    let core = spawn_core(&socket, &db, script);
    let web = spawn_web(&socket, port);
    (core, web, port, db)
}

fn boot_mock_with_shell() -> (Proc, Proc, u16, PathBuf) {
    let socket = scratch("s.sock");
    let db = scratch("core.db");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&db);
    let port = free_port();
    {
        let core = spawn_core(&socket, &db, "shell_then_read");
        let start = Instant::now();
        while !db.exists() && start.elapsed() < TIMEOUT {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(400));
        drop(core);
    }
    seed_owner_and_shell(&db);
    let core = spawn_core(&socket, &db, "shell_then_read");
    let web = spawn_web(&socket, port);
    (core, web, port, db)
}

fn boot_mock_with_shell_fail() -> (Proc, Proc, u16, PathBuf) {
    let socket = scratch("s.sock");
    let db = scratch("core.db");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&db);
    let port = free_port();
    {
        let core = spawn_core(&socket, &db, "shell_fail_then_read");
        let start = Instant::now();
        while !db.exists() && start.elapsed() < TIMEOUT {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(400));
        drop(core);
    }
    seed_owner_and_shell(&db);
    let core = spawn_core(&socket, &db, "shell_fail_then_read");
    let web = spawn_web(&socket, port);
    (core, web, port, db)
}

/// B1: web × MockEngine round trip. Echo is forbidden.
#[test]
fn web_round_trip_with_mock_reply() {
    let (_core, _web, port, _db) = boot_mock("reply");
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic mock round trip",
            "mock reply",
            TIMEOUT
        ),
        "GET must contain the mock public reply: {}",
        get_history(port)
    );
    let history = get_history(port);
    assert!(
        history.contains("\"kind\":\"agent\""),
        "reply must be an agent utterance: {history}"
    );
}

/// A4: a direct question must yield a public agent body. NO_REPLY-only is FAIL.
#[test]
fn direct_question_yields_public_reply() {
    let (_core, _web, port, _db) = boot_mock("answer_direct");
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic direct question: what is two plus two?",
            "synthetic-direct-answer",
            TIMEOUT
        ),
        "GET must contain the public agent body: {}",
        get_history(port)
    );
    let history = get_history(port);
    assert!(
        history.contains("\"kind\":\"agent\""),
        "direct question must produce an agent utterance: {history}"
    );
    let agent_only_noreply =
        history.contains("NO_REPLY") && !history.contains("synthetic-direct-answer");
    assert!(
        !agent_only_noreply,
        "NO_REPLY-only is FAIL for a direct question: {history}"
    );
}

/// A9: process-level result recovery. 2nd turn must speak the Settled/offload body.
#[test]
fn shell_result_readable_on_next_turn() {
    let (_core, _web, port, _db) = boot_mock_with_shell();
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic shell request: run date",
            "synthetic-shell-result",
            Duration::from_secs(20)
        ),
        "2nd turn must recite the Settled/offload body: {}",
        get_history(port)
    );
    let history = get_history(port);
    assert!(
        !history.contains("NO_REPLY") || history.contains("synthetic-shell-result"),
        "next turn must be able to read the shell result: {history}"
    );
}

/// G2: one tool execution must persist one row in the table #787 designates.
/// The destination is not named yet — inventing a table name is forbidden.
fn designated_tool_execution_log_table() -> Option<&'static str> {
    None
}

#[test]
fn tool_execution_writes_one_persistent_row() {
    let (_core, _web, port, _db) = boot_mock("tool_then_reply");
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic tool probe",
            "mock reply after tool result",
            TIMEOUT
        ),
        "tool_then_reply must complete one tool execution: {}",
        get_history(port)
    );
    let table = designated_tool_execution_log_table();
    assert!(
        table.is_some(),
        "tool-execution-log table is not designated by #787"
    );
}

/// A4-shape (#786): substantive body + NO_REPLY must still be a public agent body.
/// NO_REPLY-only (withheld body) is FAIL.
#[test]
fn answer_then_no_reply_delivers_substantive_body() {
    let (_core, _web, port, _db) = boot_mock("answer_then_no_reply");
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic direct question: what is two plus two?",
            "synthetic-mixed-answer",
            TIMEOUT
        ),
        "GET must contain the substantive public body: {}",
        get_history(port)
    );
    let history = get_history(port);
    assert!(
        history.contains("\"kind\":\"agent\""),
        "mixed NO_REPLY turn must produce an agent utterance: {history}"
    );
    let agent_only_noreply =
        history.contains("NO_REPLY") && !history.contains("synthetic-mixed-answer");
    assert!(
        !agent_only_noreply,
        "NO_REPLY-only is FAIL for a direct question: {history}"
    );
}

/// A9-shape (#787): a detached shell that fails must speak the failure reason next turn.
/// Public GET of the settle line is not the agent path — core-bg-read reads the offload.
#[test]
fn shell_failure_reason_readable_on_next_turn() {
    let (_core, _web, port, db) = boot_mock_with_shell_fail();
    assert!(
        post_until_history_contains(
            port,
            "test-owner",
            "synthetic shell request: run ls",
            "synthetic-shell-failure",
            Duration::from_secs(20)
        ),
        "2nd turn must recite the shell failure reason: {}",
        get_history(port)
    );
    let history = get_history(port);
    assert!(
        history.contains("許可されていない"),
        "failure reason text must reach the public reply: {history}"
    );
    assert!(
        history.contains("\"kind\":\"agent\""),
        "failure recovery must produce an agent utterance: {history}"
    );

    let store = Store::open(&db).expect("open store after failed shell");
    let place = find_room_place(&store, "room:main").expect("room:main after failed shell");
    let agent = find_agent(&store, place).expect("web-agent after failed shell");
    let failed: Vec<_> = store
        .all_activities()
        .expect("list activities")
        .into_iter()
        .filter(|a| {
            a.subject == agent
                && a.end_reason.as_deref() == Some("failed")
                && a.provenance
                    .as_ref()
                    .is_some_and(|p| p.tool_name == "core-shell")
        })
        .collect();
    assert!(
        !failed.is_empty(),
        "detached shell must leave a failed background activity"
    );
    let via_bg_read = failed.iter().find_map(|activity| {
        store
            .read_offload(agent, activity.id)
            .ok()
            .flatten()
            .filter(|row| row.body.contains("許可されていない"))
            .map(|row| row.body)
    });
    assert!(
        via_bg_read.is_some(),
        "failure reason must be retrievable via core-bg-read (offload): activities={ids:?}",
        ids = failed
            .iter()
            .map(|a| (a.id, a.label.clone(), a.end_reason.clone()))
            .collect::<Vec<_>>()
    );
}
