use std::path::{Path, PathBuf};
use std::process::Stdio;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

const INSTANCE_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const BINDING_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const ADDRESS: &str = "extgate-cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_opencrab-cli-gateway")
}

fn write_placement(dir: &Path, socket: &Path) -> PathBuf {
    let path = dir.join("placement.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "core_socket": socket,
            "instances": [{
                "instance_id": INSTANCE_ID,
                "revision": 1,
                "agent_id": "agent-a",
                "author_id": "local-operator"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn command(placement: &Path) -> Command {
    let mut command = Command::new(binary());
    command.args([
        "--placement",
        placement.to_str().unwrap(),
        "--agent",
        "agent-a",
        "--session",
        ADDRESS,
        "--mode",
        "jsonl",
        "--connect-timeout-secs",
        "1",
    ]);
    command
}

async fn mock_core(listener: UnixListener) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let hello: Value = serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
    write_json(&mut stream, &json!({"id": hello["id"], "m": "ok"}))
        .await
        .unwrap();
    write_json(
        &mut stream,
        &json!({
            "id": "bind:process",
            "m": "bind",
            "binding_id": BINDING_ID,
            "address": ADDRESS
        }),
    )
    .await
    .unwrap();
    let bind_ok: Value = serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
    assert_eq!(bind_ok, json!({"id": "bind:process", "m": "ok"}));
    while read_frame(&mut stream).await.is_ok() {}
}

fn assert_json_lines(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("stdout line must be JSON"))
        .collect()
}

#[tokio::test]
async fn invalid_argv_exits_2_with_empty_stdout() {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        Command::new(binary()).arg("--unknown").output(),
    )
    .await
    .expect("invalid argv process must exit")
    .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn startup_timeout_exits_1_with_json_only_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let placement = write_placement(dir.path(), &dir.path().join("absent.sock"));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        command(&placement).output(),
    )
    .await
    .expect("startup timeout process must exit")
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let records = assert_json_lines(&output.stdout);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "error");
    assert_eq!(records[0]["code"], "session_unavailable");
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn stdin_eof_exits_0_and_flushes_closed_record() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(mock_core(listener));
    let placement = write_placement(dir.path(), &socket);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        command(&placement).stdin(Stdio::null()).output(),
    )
    .await
    .expect("EOF process must exit")
    .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let records = assert_json_lines(&output.stdout);
    assert_eq!(records.first().unwrap()["type"], "ready");
    assert_eq!(records.last().unwrap()["type"], "closed");
    server.await.unwrap();
}

async fn spawn_waiting_process(
    placement: &Path,
) -> (
    tokio::process::Child,
    BufReader<tokio::process::ChildStdout>,
    tokio::process::ChildStdin,
) {
    let mut child = command(placement)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stdout.read_line(&mut ready),
    )
    .await
    .expect("process must emit ready")
    .unwrap();
    let value: Value = serde_json::from_str(ready.trim_end()).unwrap();
    assert_eq!(value["type"], "ready");
    (child, stdout, stdin)
}

#[tokio::test]
async fn sigint_exits_130_after_serialized_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(mock_core(listener));
    let placement = write_placement(dir.path(), &socket);
    let (mut child, mut stdout, _stdin) = spawn_waiting_process(&placement).await;
    let signal_result = unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) };
    assert_eq!(signal_result, 0);
    let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .expect("SIGINT process must exit")
        .unwrap();
    assert_eq!(status.code(), Some(130));
    let mut remaining_stdout = String::new();
    stdout.read_to_string(&mut remaining_stdout).await.unwrap();
    assert!(remaining_stdout
        .lines()
        .all(|line| serde_json::from_str::<Value>(line).is_ok()));
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .await
        .unwrap();
    assert!(!stderr.is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("mock core must observe process closure")
        .unwrap();
}

#[tokio::test]
async fn sigterm_graceful_path_exits_0_and_flushes_closed() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(mock_core(listener));
    let placement = write_placement(dir.path(), &socket);
    let (mut child, mut stdout, _stdin) = spawn_waiting_process(&placement).await;
    let signal_result = unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(signal_result, 0);
    let mut closed = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stdout.read_line(&mut closed),
    )
    .await
    .expect("SIGTERM process must emit closed")
    .unwrap();
    let closed: Value = serde_json::from_str(closed.trim_end()).unwrap();
    assert_eq!(closed, json!({"type": "closed", "reason": "requested"}));
    let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .expect("SIGTERM process must exit")
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .await
        .unwrap();
    assert!(!stderr.is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("mock core must observe process closure")
        .unwrap();
}
