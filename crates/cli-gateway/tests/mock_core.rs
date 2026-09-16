use std::path::PathBuf;
use std::time::Duration;

use opencrab_cli_gateway::args::SessionArg;
use opencrab_cli_gateway::config::{InstancePlacement, Placement};
use opencrab_cli_gateway::runtime::{run, Frontend, RunOptions};
use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

const INSTANCE_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const BINDING_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const MESSAGE_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const ADDRESS: &str = "extgate-cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn options(socket: PathBuf, session: SessionArg) -> RunOptions {
    let instance = InstancePlacement {
        instance_id: INSTANCE_ID.into(),
        revision: 1,
        agent_id: "agent-a".into(),
        author_id: "local-operator".into(),
    };
    RunOptions {
        placement: Placement {
            core_socket: socket.to_string_lossy().into_owned(),
            instances: vec![instance.clone()],
        },
        instance,
        session,
        frontend: Frontend::Jsonl,
        connect_timeout: Duration::from_secs(2),
        terminate_drain: Duration::from_secs(10),
    }
}

async fn read_value(stream: &mut UnixStream) -> Value {
    let frame = read_frame(stream).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

async fn hello(stream: &mut UnixStream) {
    let frame = read_value(stream).await;
    assert_eq!(frame["m"], "hello");
    assert_eq!(frame["instance_id"], INSTANCE_ID);
    write_json(stream, &json!({"id": frame["id"], "m": "ok"}))
        .await
        .unwrap();
}

async fn bind(stream: &mut UnixStream, binding_id: &str, address: &str) {
    write_json(
        stream,
        &json!({
            "id": "bind:1",
            "m": "bind",
            "binding_id": binding_id,
            "address": address
        }),
    )
    .await
    .unwrap();
    let response = read_value(stream).await;
    assert_eq!(response, json!({"id": "bind:1", "m": "ok"}));
}

async fn assert_said_and_accept(stream: &mut UnixStream, address: &str) {
    let said = read_value(stream).await;
    assert_eq!(said["m"], "said");
    assert_eq!(said["origin"], format!("cli:{MESSAGE_ID}"));
    assert_eq!(said["author_id"], "local-operator");
    assert_eq!(said["caller"], json!({"role": "owner"}));
    assert_eq!(said["start_turn"], true);
    assert_eq!(said["live_inbound_scope"], "all");
    assert_eq!(said["text"], "hello");
    assert!(said["attachments"].as_array().unwrap().is_empty());
    assert!(!said.as_object().unwrap().contains_key("system_context"));
    assert!(!said.as_object().unwrap().contains_key("reply_target"));
    let _ = address;
    write_json(stream, &json!({"id": said["id"], "m": "ok", "seq": 7}))
        .await
        .unwrap();
}

async fn run_case(
    socket: PathBuf,
    session: SessionArg,
    input_text: String,
) -> (
    Vec<Value>,
    anyhow::Result<opencrab_cli_gateway::runtime::Exit>,
) {
    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(16 * 1024);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        input_writer.write_all(input_text.as_bytes()).await.unwrap();
        input_writer.shutdown().await.unwrap();
    });
    let (_control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        options(socket, session),
        input_reader,
        output_writer,
        tokio::io::sink(),
        control_rx,
    ));
    let mut output = Vec::new();
    output_reader.read_to_end(&mut output).await.unwrap();
    let result = run_task.await.unwrap();
    let records = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    (records, result)
}

#[tokio::test]
async fn attached_session_sends_exact_owner_context_and_ordered_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        assert_said_and_accept(&mut stream, ADDRESS).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let input = format!(
        "{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"hello\"}}\n{{\"type\":\"shutdown\"}}\n"
    );
    let (records, result) = run_case(socket, SessionArg::Existing(ADDRESS.into()), input).await;
    result.unwrap();
    server.await.unwrap();
    assert_eq!(records[0]["type"], "ready");
    assert_eq!(records[0]["session_id"], ADDRESS);
    assert_eq!(records[1]["type"], "accepted");
    assert_eq!(records[1]["seq"], 7);
    assert_eq!(records[2], json!({"type": "closed", "reason": "requested"}));
}

#[tokio::test]
async fn disconnect_reconnects_and_never_replays_uncertain_input() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (rebound_tx, rebound_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        hello(&mut first).await;
        bind(&mut first, BINDING_ID, ADDRESS).await;
        let said = read_value(&mut first).await;
        assert_eq!(said["origin"], format!("cli:{MESSAGE_ID}"));
        drop(first);

        let (mut second, _) = listener.accept().await.unwrap();
        hello(&mut second).await;
        bind(&mut second, BINDING_ID, ADDRESS).await;
        rebound_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), read_value(&mut second))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(16 * 1024);
    use tokio::io::AsyncWriteExt;
    input_writer
        .write_all(
            format!("{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"hello\"}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let (control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        options(socket, SessionArg::Existing(ADDRESS.into())),
        input_reader,
        output_writer,
        tokio::io::sink(),
        control_rx,
    ));
    rebound_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    input_writer
        .write_all(b"{\"type\":\"shutdown\"}\n")
        .await
        .unwrap();
    input_writer.shutdown().await.unwrap();
    let mut output = Vec::new();
    output_reader.read_to_end(&mut output).await.unwrap();
    run_task.await.unwrap().unwrap();
    drop(control_tx);
    server.await.unwrap();
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let disconnected = records
        .iter()
        .position(|record| record["type"] == "connection" && record["state"] == "disconnected")
        .unwrap();
    let error = records
        .iter()
        .position(|record| record["type"] == "error" && record["code"] == "disconnect")
        .unwrap();
    let connected = records
        .iter()
        .rposition(|record| record["type"] == "connection" && record["state"] == "connected")
        .unwrap();
    let ready = records
        .iter()
        .rposition(|record| record["type"] == "ready")
        .unwrap();
    assert!(disconnected < error && error < connected && connected < ready);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "connection" && record["state"] == "disconnected")
            .count(),
        1
    );
    assert!(!records.iter().any(|record| record["type"] == "accepted"));
}

#[tokio::test]
async fn new_session_uses_generated_equal_address_and_binding_then_announces_it() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        let create = read_value(&mut stream).await;
        assert_eq!(create["m"], "create_binding");
        assert_eq!(create["session_theme"], "Terminal chat");
        let binding_id = create["binding_id"].as_str().unwrap().to_string();
        let address = create["address"].as_str().unwrap().to_string();
        assert_eq!(address, format!("extgate-{binding_id}"));
        write_json(&mut stream, &json!({"id": create["id"], "m": "ok"}))
            .await
            .unwrap();
        bind(&mut stream, &binding_id, &address).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let (records, result) = run_case(
        socket,
        SessionArg::New("Terminal chat".into()),
        "{\"type\":\"shutdown\"}\n".into(),
    )
    .await;
    result.unwrap();
    server.await.unwrap();
    assert_eq!(records[0]["type"], "ready");
    assert!(records[0]["session_id"]
        .as_str()
        .unwrap()
        .starts_with("extgate-"));
    assert_eq!(records[1]["type"], "closed");
}

#[tokio::test]
async fn maximum_jsonl_record_is_rejected_before_wire_and_next_record_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let next_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        let said = read_value(&mut stream).await;
        assert_eq!(said["origin"], format!("cli:{next_id}"));
        assert_eq!(said["text"], "after-boundary");
        write_json(&mut stream, &json!({"id": said["id"], "m": "ok", "seq": 8}))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let prefix = format!("{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"");
    let suffix = "\"}\n";
    let text_len = opencrab_cli_gateway::jsonl::MAX_LINE - prefix.len() - suffix.len();
    let mut input = format!("{prefix}{}{suffix}", "x".repeat(text_len));
    assert_eq!(input.len(), opencrab_cli_gateway::jsonl::MAX_LINE);
    input.push_str(&format!(
        "{{\"type\":\"message\",\"id\":\"{next_id}\",\"text\":\"after-boundary\"}}\n"
    ));
    input.push_str("{\"type\":\"shutdown\"}\n");
    let (records, result) = run_case(socket, SessionArg::Existing(ADDRESS.into()), input).await;
    result.unwrap();
    server.await.unwrap();
    assert!(records.iter().any(|record| {
        record["type"] == "error"
            && record["request_id"] == MESSAGE_ID
            && record["code"] == "too_large"
    }));
    assert!(records.iter().any(|record| {
        record["type"] == "accepted" && record["id"] == next_id && record["seq"] == 8
    }));
    assert!(!records
        .iter()
        .any(|record| record["type"] == "connection" && record["state"] == "disconnected"));
}

#[tokio::test]
async fn eof_without_shutdown_closes_normally() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (records, result) =
        run_case(socket, SessionArg::Existing(ADDRESS.into()), String::new()).await;
    assert_eq!(result.unwrap(), opencrab_cli_gateway::runtime::Exit::Normal);
    server.await.unwrap();
    assert_eq!(records[0]["type"], "ready");
    assert_eq!(records.last().unwrap()["type"], "closed");
}

#[tokio::test]
async fn same_uuid_retry_preserves_origin_and_core_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        for _ in 0..2 {
            let said = read_value(&mut stream).await;
            assert_eq!(said["origin"], format!("cli:{MESSAGE_ID}"));
            write_json(&mut stream, &json!({"id": said["id"], "m": "ok", "seq": 7}))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let input = format!(
        "{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"first\"}}\n{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"retry\"}}\n{{\"type\":\"shutdown\"}}\n"
    );
    let (records, result) = run_case(socket, SessionArg::Existing(ADDRESS.into()), input).await;
    result.unwrap();
    server.await.unwrap();
    let accepted: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == "accepted")
        .collect();
    assert_eq!(accepted.len(), 2);
    assert!(accepted
        .iter()
        .all(|record| record["origin"] == format!("cli:{MESSAGE_ID}") && record["seq"] == 7));
}

#[tokio::test]
async fn background_completion_no_reply_and_failure_keep_core_order() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (events_done_tx, events_done_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        let said = read_value(&mut stream).await;
        write_json(&mut stream, &json!({"id": said["id"], "m": "ok", "seq": 9}))
            .await
            .unwrap();
        let first_activity = "11111111-1111-4111-8111-111111111111";
        write_json(
            &mut stream,
            &json!({
                "m": "activity", "binding_id": BINDING_ID,
                "activity_id": first_activity, "state": "started"
            }),
        )
        .await
        .unwrap();
        write_json(
            &mut stream,
            &json!({
                "m": "activity", "binding_id": BINDING_ID,
                "activity_id": first_activity, "state": "ended",
                "silent_origins": [format!("cli:{MESSAGE_ID}")]
            }),
        )
        .await
        .unwrap();
        let background = "22222222-2222-4222-8222-222222222222";
        write_json(
            &mut stream,
            &json!({
                "m": "activity", "binding_id": BINDING_ID,
                "activity_id": background, "state": "started"
            }),
        )
        .await
        .unwrap();
        write_json(
            &mut stream,
            &json!({
                "id": "delivery:background", "m": "say", "binding_id": BINDING_ID,
                "payload": {"text": "background result"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(read_value(&mut stream).await["m"], "ok");
        write_json(
            &mut stream,
            &json!({
                "m": "activity", "binding_id": BINDING_ID,
                "activity_id": background, "state": "ended",
                "completed_target": "delivery:background", "silent_origins": []
            }),
        )
        .await
        .unwrap();
        let failed = "33333333-3333-4333-8333-333333333333";
        write_json(
            &mut stream,
            &json!({
                "m": "activity", "binding_id": BINDING_ID,
                "activity_id": failed, "state": "started"
            }),
        )
        .await
        .unwrap();
        write_json(
            &mut stream,
            &json!({
                "m": "turn_failed", "binding_id": BINDING_ID,
                "origin": format!("cli:{MESSAGE_ID}")
            }),
        )
        .await
        .unwrap();
        events_done_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(16 * 1024);
    use tokio::io::AsyncWriteExt;
    input_writer
        .write_all(
            format!("{{\"type\":\"message\",\"id\":\"{MESSAGE_ID}\",\"text\":\"start\"}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let (_control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        options(socket, SessionArg::Existing(ADDRESS.into())),
        input_reader,
        output_writer,
        tokio::io::sink(),
        control_rx,
    ));
    events_done_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    input_writer
        .write_all(b"{\"type\":\"shutdown\"}\n")
        .await
        .unwrap();
    input_writer.shutdown().await.unwrap();
    let mut output = Vec::new();
    output_reader.read_to_end(&mut output).await.unwrap();
    run_task.await.unwrap().unwrap();
    server.await.unwrap();
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let visible: Vec<String> = records
        .iter()
        .filter_map(|record| match record["type"].as_str()? {
            "activity" => Some(format!("activity:{}", record["state"].as_str().unwrap())),
            "completed_no_reply" | "message" | "completed" | "turn_failed" => {
                Some(record["type"].as_str().unwrap().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        visible,
        vec![
            "activity:started",
            "activity:ended",
            "completed_no_reply",
            "activity:started",
            "message",
            "activity:ended",
            "completed",
            "activity:started",
            "turn_failed",
        ]
    );
}

#[tokio::test]
async fn repl_startup_error_uses_stderr_and_never_draws_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(
        dir.path().join("absent.sock"),
        SessionArg::Existing(ADDRESS.into()),
    );
    opts.frontend = Frontend::Repl;
    opts.connect_timeout = Duration::from_millis(20);
    let (stdout_writer, mut stdout_reader) = tokio::io::duplex(4096);
    let (stderr_writer, mut stderr_reader) = tokio::io::duplex(4096);
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout_reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr_reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let (_control_tx, control_rx) = mpsc::channel(1);
    assert!(run(
        opts,
        tokio::io::empty(),
        stdout_writer,
        stderr_writer,
        control_rx,
    )
    .await
    .is_err());
    let stdout = String::from_utf8(stdout_task.await.unwrap()).unwrap();
    let stderr = String::from_utf8(stderr_task.await.unwrap()).unwrap();
    assert!(stdout.is_empty());
    assert_eq!(stderr, "error: session_unavailable\n");
    assert!(!stdout.contains("you> "));
}

#[tokio::test]
async fn repl_runtime_disconnect_error_uses_stderr_without_error_prompt_redraw() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        let _said = read_value(&mut stream).await;
        drop(stream);
    });
    let mut opts = options(socket, SessionArg::Existing(ADDRESS.into()));
    opts.frontend = Frontend::Repl;
    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (stdout_writer, mut stdout_reader) = tokio::io::duplex(4096);
    let (stderr_writer, mut stderr_reader) = tokio::io::duplex(4096);
    use tokio::io::AsyncWriteExt;
    input_writer.write_all(b"hello\n:quit\n").await.unwrap();
    input_writer.shutdown().await.unwrap();
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout_reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr_reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let (_control_tx, control_rx) = mpsc::channel(1);
    run(opts, input_reader, stdout_writer, stderr_writer, control_rx)
        .await
        .unwrap();
    server.await.unwrap();
    let stdout = String::from_utf8(stdout_task.await.unwrap()).unwrap();
    let stderr = String::from_utf8(stderr_task.await.unwrap()).unwrap();
    assert!(stdout.contains("Connected: agent-a"));
    assert!(stdout.contains("disconnected"));
    assert!(!stdout.contains("error: disconnect"));
    assert_eq!(stderr, "error: disconnect\n");
}

struct NeverWriter;

impl tokio::io::AsyncWrite for NeverWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::task::Poll::Pending
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Pending
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn sigterm_bound_includes_blocked_stdout_writer_completion() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        bound_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let mut opts = options(socket, SessionArg::Existing(ADDRESS.into()));
    opts.terminate_drain = Duration::from_millis(30);
    let (_input_writer, input_reader) = tokio::io::duplex(64);
    let (control_tx, control_rx) = mpsc::channel(1);
    let run_task = tokio::spawn(run(
        opts,
        input_reader,
        NeverWriter,
        tokio::io::sink(),
        control_rx,
    ));
    bound_rx.await.unwrap();
    control_tx
        .send(opencrab_cli_gateway::runtime::Control::Terminate)
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), run_task)
        .await
        .expect("short configured drain must terminate")
        .unwrap()
        .unwrap_err();
    assert!(result.to_string().contains("termination drain timed out"));
    server.await.unwrap();
}

#[tokio::test]
async fn interrupt_control_flushes_and_returns_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hello(&mut stream).await;
        bind(&mut stream, BINDING_ID, ADDRESS).await;
        bound_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let (_input_writer, input_reader) = tokio::io::duplex(64);
    let (control_tx, control_rx) = mpsc::channel(1);
    let task = tokio::spawn(run(
        options(socket, SessionArg::Existing(ADDRESS.into())),
        input_reader,
        tokio::io::sink(),
        tokio::io::sink(),
        control_rx,
    ));
    bound_rx.await.unwrap();
    control_tx
        .send(opencrab_cli_gateway::runtime::Control::Interrupt)
        .await
        .unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("interrupt control must finish")
        .unwrap()
        .unwrap();
    assert_eq!(exit, opencrab_cli_gateway::runtime::Exit::Interrupted);
    server.await.unwrap();
}
